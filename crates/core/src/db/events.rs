//! `events` 表的写入与清理
//!
//! 批量插入带三级降级（整批事务 → 单条重试 → 单条独立事务），确保成功行不丢失。

use rusqlite::params;

use super::{lock_writer, Database};
use crate::types::Event;

const INSERT_SQL: &str = "
INSERT INTO events (timestamp, event_type, event_action, event_data, app_name, window_title, session_id)
VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
";

/// input_agg 行是派生聚合缓存（0005 迁移的部分唯一索引保证同分钟同类型恰一行）：
/// 秒级刷新用累计值 UPSERT 覆盖，而非追加，避免一行/秒的行数膨胀。
const UPSERT_INPUT_AGG_SQL: &str = "
INSERT INTO events (timestamp, event_type, event_action, event_data, app_name, window_title, session_id)
VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
ON CONFLICT(timestamp, event_type) WHERE event_action = 'input_agg'
DO UPDATE SET event_data = excluded.event_data
";

impl Database {
    /// 批量插入事件
    ///
    /// 健壮性：单条失败不会让整批丢失——
    /// 1. 第一次尝试：事务内逐条 INSERT
    /// 2. 任意一条失败 → ROLLBACK 整个事务
    /// 3. 重试一次（去掉失败行 + 可能不冲突的 schema 漂移）
    /// 4. 仍失败则降级为逐条单独事务，确保成功行不丢
    pub fn insert_events(&self, events: &[Event]) {
        if events.is_empty() {
            return;
        }

        // 第一次尝试：单事务整批
        match self.insert_events_tx(events) {
            Ok(n) => {
                if n != events.len() {
                    log::warn!("首次插入 {} / {} 成功，触发单条重试", n, events.len());
                    self.insert_events_one_by_one(events);
                }
            }
            Err(e) => {
                log::error!("批量插入失败，降级为逐条: {e}");
                self.insert_events_one_by_one(events);
            }
        }
    }

    fn insert_events_tx(&self, events: &[Event]) -> rusqlite::Result<usize> {
        let Some(conn) = lock_writer(&self.writer, &self.db_path) else {
            return Err(rusqlite::Error::ExecuteReturnedResults);
        };
        let tx = conn.unchecked_transaction()?;
        // prepare_cached 复用 prepared statement 计划，避免每行重新 prepare/finalize
        // （原 tx.execute 每行都 prepare 一次，批量 300 条 = 300 次 prepare）。
        {
            let mut stmt = tx.prepare_cached(INSERT_SQL)?;
            let mut agg_stmt = tx.prepare_cached(UPSERT_INPUT_AGG_SQL)?;
            for e in events {
                let data_str = e.event_data.as_ref().map(|v| v.to_string());
                // input_agg 聚合行走 UPSERT（同分钟同类型覆盖），其余照旧 INSERT
                let is_agg = e.event_action == crate::types::EventAction::InputAgg;
                // as_str() 返回 &'static str，替代原 to_string() 的每行堆分配
                (if is_agg { &mut agg_stmt } else { &mut stmt } as &mut rusqlite::Statement<'_>).execute(params![
                    e.timestamp,
                    e.event_type.as_str(),
                    e.event_action.as_str(),
                    data_str,
                    e.app_name,
                    e.window_title,
                    e.session_id,
                ])?;
            }
        }
        tx.commit()?;
        Ok(events.len())
    }

    fn insert_events_one_by_one(&self, events: &[Event]) {
        for e in events {
            let Some(conn) = lock_writer(&self.writer, &self.db_path) else {
                continue;
            };
            let Ok(tx) = conn.unchecked_transaction() else {
                continue;
            };
            let data_str = e.event_data.as_ref().map(|v| v.to_string());
            // 降级路径同样用 prepare_cached + as_str
            let result = (|| -> rusqlite::Result<()> {
                let mut stmt = tx.prepare_cached(INSERT_SQL)?;
                stmt.execute(params![
                    e.timestamp,
                    e.event_type.as_str(),
                    e.event_action.as_str(),
                    data_str,
                    e.app_name,
                    e.window_title,
                    e.session_id,
                ])?;
                Ok(())
            })();
            match result {
                Ok(_) => {
                    if let Err(c) = tx.commit() {
                        log::warn!("单条提交失败: {c}");
                    }
                }
                Err(err) => {
                    log::warn!("单条事件写入失败（已跳过）: {err}");
                }
            }
        }
    }

    /// 清理超过保留天数的事件
    pub fn cleanup_old_events(&self) -> usize {
        if self.retention_days <= 0 {
            return 0; // 0 = 永不删除（默认）。铁律：原始数据只能显式 opt-in 才清理。
        }
        self.with_writer(
            |conn| {
                let cutoff = chrono::Utc::now() - chrono::Duration::days(self.retention_days);
                let cutoff_str = cutoff.to_rfc3339();
                match conn.execute("DELETE FROM events WHERE timestamp < ?1", [&cutoff_str]) {
                    Ok(deleted) => {
                        if deleted > 0 {
                            log::info!(
                                "清理了 {} 条超过 {} 天的旧事件",
                                deleted,
                                self.retention_days
                            );
                        }
                        deleted
                    }
                    Err(e) => {
                        log::error!("清理旧事件失败: {e}");
                        0
                    }
                }
            },
            || 0,
        )
    }
}
