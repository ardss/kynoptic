//! `sessions` 表的生命周期管理（开启/结束/清理/幽灵清扫）
//!
//! 设计说明：`end_session` 不再反向调用 `daily_agg::recompute_day`
//! （原 db.rs:436 处的数据层 → 聚合层循环依赖）。
//! 当日聚合的刷新由上层（采集器关闭流程）显式触发，保持数据层只依赖 schema。

use rusqlite::params;

use super::{lock_writer, Database};

impl Database {
    pub fn start_session(&self) -> i64 {
        self.with_writer(
            |conn| {
                let now = chrono::Utc::now().to_rfc3339();
                conn.execute("INSERT INTO sessions (start_time) VALUES (?1)", [&now])
                    .ok();
                conn.last_insert_rowid()
            },
            || 0,
        )
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

        if open_ids.len() <= 1 {
            return 0; // 0 或 1 个未关闭 session，无需清扫
        }

        // 保留最后一个（id 最大 = 最近的），其余视为幽灵
        let ghost_ids: Vec<i64> = open_ids.iter().rev().skip(1).copied().collect();
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
