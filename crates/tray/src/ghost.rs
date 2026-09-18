//! tray 启动时的幽灵 session 清扫（双 open 修复）。
//!
//! 根因：`core::db::close_ghost_sessions` 的语义是"保留 start_time 最新的那个
//! open session（视为当前）"。崩溃/强杀后恰好留下 **1 个** open 幽灵时，它
//! 被当成"当前 session"保留，随后 `start_session` 又新建一个——重启后库里
//! 永远并存 2 个 `end_time IS NULL` 的 session（watchdog 反复拉起也一样）。
//!
//! 修法：tray 在派发 CollectorCmd::Start **之前**（采集器 open DB 之前）把
//! 除"本进程刚建的"之外的全部 open session 闭合——即启动时刻的所有 open
//! session 全部补 end_time（本进程一个都还没建，等价于全清）。之后的
//! `close_ghost_sessions` 在采集器启动路径里变成 no-op（0 个 open），
//! `start_session` 创建的将是唯一的 open session。
//!
//! 闭合语义与 core 现有一致：end_time 取该 session 最后一条事件的
//! timestamp，没有事件则回退 session 的 start_time；同时补 total_events。

use std::path::Path;

/// 闭合 db 里全部 `end_time IS NULL` 的 session。返回闭合数量。
///
/// 审查 P2：返回值升级为 Result——"确实没有幽灵"（Ok(0)）与"DB 被锁 /
/// 打不开 / 清扫中途出错"（Err，携带 rusqlite 错误消息）是两种完全不同的
/// 状况，旧实现一律静默 0，库损坏/被占用时托盘启动无从告警。db 不存在仍
/// 返回 Ok(0)——全新首装是正常路径。注意：不存在时直接返回——rusqlite 的
/// Connection::open 会建出空库文件，而建库是采集器职责，绝不能在这里
/// touch 出一个空库。
pub fn close_all_open_sessions(db_path: &Path) -> Result<usize, String> {
    if !db_path.exists() {
        return Ok(0);
    }
    let conn = rusqlite::Connection::open(db_path).map_err(|e| format!("open 失败: {e}"))?;
    let _ = conn.execute_batch("PRAGMA busy_timeout=5000;");
    let open_ids = conn
        .prepare("SELECT id FROM sessions WHERE end_time IS NULL ORDER BY id")
        .and_then(|mut s| {
            s.query_map([], |r| r.get::<_, i64>(0))
                .map(|rows| rows.filter_map(|x| x.ok()).collect::<Vec<i64>>())
        })
        .map_err(|e| format!("查询 open session 失败（库被锁/表损坏?）: {e}"))?;
    let mut closed = 0usize;
    for sid in open_ids {
        let end_time: Option<String> = conn
            .query_row(
                "SELECT timestamp FROM events WHERE session_id = ?1 ORDER BY timestamp DESC LIMIT 1",
                [sid],
                |r| r.get(0),
            )
            .ok();
        let end_time = end_time.unwrap_or_else(|| {
            conn.query_row(
                "SELECT start_time FROM sessions WHERE id = ?1",
                [sid],
                |r| r.get::<_, String>(0),
            )
            .unwrap_or_else(|_| chrono::Utc::now().to_rfc3339())
        });
        let n_events: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM events WHERE session_id = ?1",
                [sid],
                |r| r.get(0),
            )
            .unwrap_or(0);
        // 审查 P2：单条 UPDATE 失败（库被锁/磁盘满）不再是静默跳过——
        // 半清不除等于双 open 修复失效，必须上报给调用方告警。
        let updated = conn
            .execute(
                "UPDATE sessions SET end_time = ?1, total_events = ?2 WHERE id = ?3 AND end_time IS NULL",
                rusqlite::params![end_time, n_events, sid],
            )
            .map_err(|e| format!("闭合 session {sid} 失败: {e}"))?;
        if updated > 0 {
            closed += 1;
        }
    }
    if closed > 0 {
        log::info!("启动清扫：闭合了 {closed} 个遗留 open session");
    }
    Ok(closed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_db(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "kynoptic-tray-ghost-test-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("kynoptic.db")
    }

    fn open_count(conn: &rusqlite::Connection) -> i64 {
        conn.query_row(
            "SELECT COUNT(*) FROM sessions WHERE end_time IS NULL",
            [],
            |r| r.get(0),
        )
        .unwrap()
    }

    /// 实测路径回归：强杀留下 1 个 open 幽灵 → tray 清扫 → 采集器启动
    /// （close_ghost_sessions + start_session）→ 重启后 sessions 表最多
    /// 1 个 open。修复前该场景必然双 open（幽灵被 close_ghost_sessions
    /// 当作"当前"保留，start_session 再建第二个）。
    #[test]
    fn restart_leaves_at_most_one_open_session() {
        let db = temp_db("restart");
        // —— 第一次"运行"：崩溃，遗留 1 个 open session（含 2 条事件）——
        {
            let d = kynoptic_core::db::Database::open(db.to_str().unwrap()).unwrap();
            let sid = d.start_session();
            let conn = d.reader();
            for i in 0..2 {
                conn.execute(
                    "INSERT INTO events (timestamp, event_type, event_action, event_data, app_name, window_title, session_id) VALUES (?1,'keyboard','press',NULL,NULL,NULL,?2)",
                    rusqlite::params![format!("2026-09-17T0{i}:00:00+00:00"), sid],
                )
                .unwrap();
            }
            // 不 end_session：模拟强杀
        }
        // —— tray 启动清扫（本修复）——
        assert_eq!(close_all_open_sessions(&db).unwrap(), 1);
        // —— 采集器启动路径（与 core collector 相同顺序）——
        {
            let d = kynoptic_core::db::Database::open(db.to_str().unwrap()).unwrap();
            d.close_ghost_sessions();
            d.start_session();
        }
        let conn = rusqlite::Connection::open(&db).unwrap();
        assert!(open_count(&conn) <= 1, "重启后 sessions 表最多 1 个 open");
        let _ = std::fs::remove_dir_all(db.parent().unwrap());
    }

    /// 多个幽灵（历史 bug 已积累双 open）一次全清。
    #[test]
    fn multiple_ghosts_all_closed_with_last_event_end_time() {
        let db = temp_db("multi");
        {
            let conn = rusqlite::Connection::open(&db).unwrap();
            conn.execute_batch(kynoptic_core::db::SCHEMA).unwrap();
            let _ = kynoptic_core::db::run_migrations(&conn);
            conn.execute(
                "INSERT INTO sessions (start_time) VALUES ('2026-09-16T01:00:00+00:00')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO sessions (start_time) VALUES ('2026-09-16T02:00:00+00:00')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO events (timestamp, event_type, event_action, event_data, session_id) VALUES ('2026-09-16T01:30:00+00:00','keyboard','press',NULL,1)",
                [],
            )
            .unwrap();
        }
        assert_eq!(close_all_open_sessions(&db).unwrap(), 2);
        let conn = rusqlite::Connection::open(&db).unwrap();
        assert_eq!(open_count(&conn), 0);
        // end_time = 该 session 最后一条事件时间；无事件的回退 start_time
        let e1: String = conn
            .query_row("SELECT end_time FROM sessions WHERE id = 1", [], |r| {
                r.get(0)
            })
            .unwrap();
        let e2: String = conn
            .query_row("SELECT end_time FROM sessions WHERE id = 2", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(e1, "2026-09-16T01:30:00+00:00");
        assert_eq!(e2, "2026-09-16T02:00:00+00:00");
        let n1: i64 = conn
            .query_row("SELECT total_events FROM sessions WHERE id = 1", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(n1, 1);
        let _ = std::fs::remove_dir_all(db.parent().unwrap());
    }

    /// db 不存在（全新首装）：Ok(0)，不建库不报错。
    #[test]
    fn missing_db_is_noop() {
        let db = temp_db("missing");
        assert_eq!(close_all_open_sessions(&db).unwrap(), 0);
        assert!(!db.exists(), "清扫不得建库（建库是采集器职责）");
        let _ = std::fs::remove_dir_all(db.parent().unwrap());
    }
}
