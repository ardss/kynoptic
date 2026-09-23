//! `sessions` 表的生命周期管理（开启/结束/清理/幽灵清扫）
//!
//! 设计说明：`end_session` 不再反向调用 `daily_agg::recompute_day`
//! （原 db.rs:436 处的数据层 → 聚合层循环依赖）。
//! 当日聚合的刷新由上层（采集器关闭流程）显式触发，保持数据层只依赖 schema。

use rusqlite::params;
use std::sync::atomic::Ordering;

use super::{lock_writer, Database};

impl Database {
    /// 当前采集会话 id（0 = 无会话）。writer 落库前用它补盖 event.session_id。
    pub fn current_session_id(&self) -> i64 {
        self.current_session.load(Ordering::Relaxed)
    }

    pub fn start_session(&self) -> i64 {
        let id = self.with_writer(
            |conn| {
                let now = chrono::Utc::now().to_rfc3339();
                // P0 修复：INSERT 失败（磁盘满/只读/触发器中止）时不得读
                // last_insert_rowid——那会拿到写连接上一次任意表插入的残留
                // rowid，被当作新 session id 后幽灵清扫（end_time IS NULL）
                // 永久失明。失败时 log::error 并返回 0（= 无会话），writer
                // 不补盖 session_id，数据保持 NULL 可追溯。
                match conn.execute("INSERT INTO sessions (start_time) VALUES (?1)", [&now]) {
                    Ok(_) => conn.last_insert_rowid(),
                    Err(e) => {
                        log::error!("start_session INSERT 失败，本会话不登记 session id: {e}");
                        0
                    }
                }
            },
            || 0,
        );
        // 登记当前会话 id（0 = 无会话），供 writer 落库时补盖 event.session_id。
        // 此前该列全为 NULL，ghost 清扫/total_events 全部失效。
        self.current_session.store(id, Ordering::Relaxed);
        id
    }

    /// 某会话在 events 表中的实际行数（COUNT(*) 口径）。
    ///
    /// 审查 33-F6：sessions.total_events 的双口径统一——优雅关停此前写
    /// total_written（把每秒一次的 input_agg UPSERT 逐次累加，Minute 粒度下
    /// 与实际行数可差约 60 倍），ghost 清扫写 COUNT(*)。现在优雅关停也用
    /// 本查询取数，两种结束路径天然一致。
    pub fn session_event_count(&self, session_id: i64) -> i64 {
        self.reader()
            .query_row(
                "SELECT COUNT(*) FROM events WHERE session_id = ?1",
                params![session_id],
                |r| r.get(0),
            )
            .unwrap_or(0)
    }

    /// 结束会话。注意：不再在此触发 daily_agg 重算（消除 db → daily_agg 反向依赖）。
    /// 如需刷新当日聚合，调用方应在结束后显式调用 `daily_agg::recompute_day`。
    pub fn end_session(&self, session_id: i64, total_events: i64, idle_seconds: f64) {
        self.with_writer(
            |conn| {
                let now = chrono::Utc::now().to_rfc3339();
                if let Err(e) = conn.execute(
                    "UPDATE sessions SET end_time = ?1, total_events = ?2, idle_seconds = ?3 WHERE id = ?4",
                    params![now, total_events, idle_seconds, session_id],
                ) {
                    log::warn!("end_session 失败: {e}");
                }
            },
            || {},
        )
    }

    /// 清理已结束的旧会话
    pub fn cleanup_old_sessions(&self) -> usize {
        self.with_writer(
            |conn| {
                // Wave22 P0：retention=0（默认）= 永不删除——cleanup_old_events
                // 有此守卫而这里漏了，导致每次维护把全部已结束 session 删光，
                // 直接违反"永不删数据"铁律。
                if self.retention_days <= 0 {
                    return 0;
                }
                let cutoff = chrono::Utc::now() - chrono::Duration::days(self.retention_days);
                let cutoff_str = cutoff.to_rfc3339();
                conn.execute(
                    "DELETE FROM sessions WHERE end_time IS NOT NULL AND end_time < ?1",
                    [&cutoff_str],
                )
                .unwrap_or(0)
            },
            || 0,
        )
    }

    /// 启动时清扫上次未关闭的 session（应用崩溃 / 强杀 / 断电时留下）。
    /// 保留 start_time 最新的那个 end_time IS NULL 的 session（视为当前），
    /// 其余补上 end_time 和 total_events。返回被清扫的 session 数量。
    pub fn close_ghost_sessions(&self) -> usize {
        let Some(conn) = lock_writer(&self.writer, &self.db_path) else {
            return 0;
        };

        // 取所有未关闭的 session，按 id 升序
        let open_ids: Vec<i64> = match conn
            .prepare("SELECT id FROM sessions WHERE end_time IS NULL ORDER BY id")
            .and_then(|mut s| {
                s.query_map([], |r| r.get::<_, i64>(0))
                    .map(|mapped| mapped.filter_map(|x| x.ok()).collect())
            }) {
            Ok(v) => v,
            Err(e) => {
                log::error!("读取未关闭 session 失败: {e}");
                return 0;
            }
        };

        // Wave22 P1 定案：启动清扫关闭**全部** open session——唯一调用方
        //（collector 启动）清扫后立即 start_session 开新会话，旧的"保留
        // 最新一个"会在每轮重启滞留一个永不关闭的僵尸 open session。
        // 此时的 open session 全部属于上次进程，无一例外是幽灵。
        let ghost_ids: Vec<i64> = open_ids.clone();
        let mut closed = 0usize;

        for sid in &ghost_ids {
            // 取该 session 最后一条事件的时间作为 end_time
            let end_time: Option<String> = conn
                .query_row(
                    "SELECT timestamp FROM events WHERE session_id = ?1 ORDER BY timestamp DESC LIMIT 1",
                    params![sid],
                    |r| r.get(0),
                )
                .ok();
            // 取该 session 的事件总数
            let n_events: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM events WHERE session_id = ?1",
                    params![sid],
                    |r| r.get(0),
                )
                .unwrap_or(0);

            // 兜底 end_time：如果没事件，用 session 的 start_time
            let end_time_str = match end_time {
                Some(t) => t,
                None => conn
                    .query_row(
                        "SELECT start_time FROM sessions WHERE id = ?1",
                        params![sid],
                        |r| r.get::<_, String>(0),
                    )
                    .unwrap_or_else(|_| chrono::Utc::now().to_rfc3339()),
            };

            if let Err(e) = conn.execute(
                "UPDATE sessions SET end_time = ?1, total_events = ?2 WHERE id = ?3 AND end_time IS NULL",
                params![end_time_str, n_events, sid],
            ) {
                log::warn!("清扫幽灵 session {sid} 失败: {e}");
                continue;
            }
            closed += 1;
        }

        if closed > 0 {
            log::info!("启动清扫：关闭了 {closed} 个上次的幽灵 session");
        }
        closed
    }
}
