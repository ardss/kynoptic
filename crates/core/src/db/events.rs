//! `events` 表的写入与清理
//!
//! 批量插入带三级降级（整批事务 → 单条重试 → 单条独立事务），确保成功行不丢失。
//!
//! 聚合一致性（审查 P1）：[`Database::insert_events_with_agg`] 把 events 落库与
//! agg_minute/agg_daily 增量维护包进**同一个事务**——旧实现两者是独立事务，
//! 中间 kill 会留下"events 有 agg 无"的欠聚合（backfill_needed 只在 agg 全空时
//! 触发，永不自愈）。事务化后窗口消失；[`Database::insert_events`] 保持只插
//! events 的旧语义，供测试与不需要聚合维护的调用方使用。

use rusqlite::params;
use rusqlite::Connection;

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

/// 写失败计数（P0）：磁盘写满/写失败时旧实现只 log::warn（且"降级逐条→跳过"
/// 后无任何聚合观测），告警看门狗只看通道满载计数，写失败完全静默。所有
/// 最终失败的写路径（整批失败、单条降级失败、提交失败）都累加此计数，由
/// collector 的 DropWatchdog 周期性读取并告警。
fn note_write_failure(n: u64) {
    crate::collector::WRITE_FAILURES.fetch_add(n, std::sync::atomic::Ordering::Relaxed);
}

/// 降级逐条后判断"整批全败"（rowid 全 0 即无一成功落库），供整批兜底重试。
fn all_failed(rowids: &[i64], expected: usize) -> bool {
    rowids.len() == expected && rowids.iter().all(|&r| r == 0)
}

/// 时间戳规范化（审查 HIGH：格式漂移防护）：统一改写为 UTC `+00:00` 形的
/// RFC3339。events.timestamp 的全部范围谓词是字符串比较，一旦带非 +00:00
/// 后缀（采集器 bug 或外部导入写成 '+08:00'/'Z'），字典序不再等于时间序，
/// 事件会被静默计入错误的"今日"或直接丢弃。可解析的时间戳就地归一化；
/// 不可解析的保持原样（不阻塞落库，由读侧既有容错兜底）。
fn normalize_timestamp(ts: &str) -> String {
    match chrono::DateTime::parse_from_rfc3339(ts) {
        // to_rfc3339 输出固定 +00:00 后缀，字典序 == 时间序
        Ok(t) => t.with_timezone(&chrono::Utc).to_rfc3339(),
        Err(_) => ts.to_string(),
    }
}

/// 单条事件落库（input_agg 行走 UPSERT，其余 INSERT）。返回该行的 events
/// rowid（input_agg 聚合行无稳定新 rowid，返回 0，与旧 insert_events 语义一致）。
fn execute_event(tx: &Connection, e: &Event) -> rusqlite::Result<i64> {
    let data_str = e.event_data.as_ref().map(|v| v.to_string());
    // 审查 HIGH：写库统一规范化时间戳（见 normalize_timestamp），
    // 保证字符串范围谓词（timestamp >= ?1 AND < ?2）语义正确
    let ts = normalize_timestamp(&e.timestamp);
    // input_agg 聚合行走 UPSERT（同分钟同类型覆盖），其余照旧 INSERT
    let is_agg = e.event_action == crate::types::EventAction::InputAgg;
    // prepare_cached 复用 prepared statement 计划，避免每行重新 prepare/finalize
    let mut stmt = tx.prepare_cached(if is_agg {
        UPSERT_INPUT_AGG_SQL
    } else {
        INSERT_SQL
    })?;
    stmt.execute(params![
        ts,
        e.event_type.as_str(),
        e.event_action.as_str(),
        data_str,
        e.app_name,
        e.window_title,
        e.session_id,
    ])?;
    Ok(if is_agg { 0 } else { tx.last_insert_rowid() })
}

impl Database {
    /// 批量插入事件（只插 events，不做聚合维护）。
    ///
    /// 健壮性：单条失败不会让整批丢失——
    /// 1. 第一次尝试：事务内逐条 INSERT
    /// 2. 任意一条失败 → ROLLBACK 整个事务
    /// 3. 重试一次（去掉失败行 + 可能不冲突的 schema 漂移）
    /// 4. 仍失败则降级为逐条单独事务，确保成功行不丢
    ///
    /// 返回与 `events` 一一对应的 rowid（写入失败的行记 0，供 agg 增量维护
    /// 的 max_event_rowid 幂等防护使用）。
    pub fn insert_events(&self, events: &[Event]) -> Vec<i64> {
        if events.is_empty() {
            return Vec::new();
        }

        // 第一次尝试：单事务整批（事务内任一条失败即整体 Err → 降级逐条）
        match self.insert_events_tx(events) {
            Ok(rowids) => rowids,
            Err(e) => {
                note_write_failure(1);
                log::error!("批量插入失败，降级为逐条: {e}");
                self.insert_events_one_by_one(events)
            }
        }
    }

    /// 批量插入事件并在**同一事务**内维护聚合缓存（writer 主路径）。
    ///
    /// 审查 P1：events 落库与 agg 增量 UPSERT 原为两个独立事务，中间 kill 即
    /// 欠聚合且无法自愈。合并后两者同生共死：要么 events+agg 都提交，要么都
    /// 回滚（回滚后走逐条降级，同样每条带聚合维护）。
    /// 返回与 `events` 一一对应的 rowid（同 [`Database::insert_events`]）。
    pub fn insert_events_with_agg(&self, events: &[Event]) -> Vec<i64> {
        if events.is_empty() {
            return Vec::new();
        }

        match self.insert_events_with_agg_tx(events) {
            Ok(rowids) => rowids,
            Err(first) => {
                // 审查 P1：外部写者（backfill/agg-heal 分块）可能持有写锁超过
                // writer 的 busy_timeout，整批事务 SQLITE_BUSY 失败并不代表数据
                // 有问题；这批事件已从通道 pop 出，降级逐条若同样全撞 busy 就
                // 是永久丢失（丢已 pop 的事件即数据丢失，非可丢弃错误）。先睡
                // 500ms 让外部写者的当前块写完，整批重试一次，仍失败才降级逐条。
                log::warn!("批量插入（含聚合维护）失败，500ms 后整批重试一次: {first}");
                std::thread::sleep(std::time::Duration::from_millis(500));
                match self.insert_events_with_agg_tx(events) {
                    Ok(rowids) => rowids,
                    Err(e) => {
                        note_write_failure(1);
                        log::error!("批量插入（含聚合维护）重试仍失败，降级为逐条: {e}");
                        let rowids = self.insert_events_with_agg_one_by_one(events);
                        // 审查 P1：逐条也全败（busy 风暴未退，整批已 pop 出通道）
                        // → 最后再整批兜底重试一次，避免整块事件永久丢失。
                        // （纯 input_agg 批成功时 rowid 也全为 0，会多一次幂等
                        // UPSERT 重试，无害。）
                        if all_failed(&rowids, events.len()) {
                            std::thread::sleep(std::time::Duration::from_millis(500));
                            match self.insert_events_with_agg_tx(events) {
                                Ok(rowids2) => {
                                    log::info!(
                                        "逐条降级全败后整批兜底重试成功（{} 条）",
                                        events.len()
                                    );
                                    return rowids2;
                                }
                                Err(e2) => {
                                    log::error!(
                                        "整批兜底重试仍失败，{} 条事件丢失: {e2}",
                                        events.len()
                                    );
                                }
                            }
                        }
                        rowids
                    }
                }
            }
        }
    }

    fn insert_events_tx(&self, events: &[Event]) -> rusqlite::Result<Vec<i64>> {
        let Some(conn) = lock_writer(&self.writer, &self.db_path) else {
            return Err(rusqlite::Error::ExecuteReturnedResults);
        };
        let tx = conn.unchecked_transaction()?;
        let mut rowids: Vec<i64> = Vec::with_capacity(events.len());
        for e in events {
            rowids.push(execute_event(&tx, e)?);
        }
        tx.commit()?;
        Ok(rowids)
    }

    fn insert_events_with_agg_tx(&self, events: &[Event]) -> rusqlite::Result<Vec<i64>> {
        let Some(conn) = lock_writer(&self.writer, &self.db_path) else {
            return Err(rusqlite::Error::ExecuteReturnedResults);
        };
        let tx = conn.unchecked_transaction()?;
        let mut rowids: Vec<i64> = Vec::with_capacity(events.len());
        for e in events {
            let rowid = execute_event(&tx, e)?;
            // 聚合增量与事件写入同事务：max_event_rowid 守卫保留作为幂等兜底
            // （事务化后"回填重算与增量并发"的交错窗口消失，重复投递仍被跳过）
            super::agg::apply_event(&tx, e, rowid)?;
            rowids.push(rowid);
        }
        tx.commit()?;
        Ok(rowids)
    }

    /// 降级路径公共体：单条独立事务（可选附带聚合维护），失败计数并跳过。
    ///
    /// 契约：返回 Vec 与 `events` **一一对应**——失败的行 push 0（rowid 0 在
    /// agg 增量维护中被 max_event_rowid 守卫跳过，不会重复累计）。交叉审查
    /// P4 修复：此前 lock_writer / 事务创建失败的 continue 路径不 push，返回
    /// Vec 短于 events，违反 doc 契约（调用方按索引 zip 会错位）。
    fn insert_one_by_one_impl(&self, events: &[Event], with_agg: bool) -> Vec<i64> {
        let mut rowids: Vec<i64> = Vec::with_capacity(events.len());
        for e in events {
            let Some(conn) = lock_writer(&self.writer, &self.db_path) else {
                note_write_failure(1);
                rowids.push(0);
                continue;
            };
            let Ok(tx) = conn.unchecked_transaction() else {
                note_write_failure(1);
                rowids.push(0);
                continue;
            };
            // 降级路径 input_agg 行同样必须走 UPSERT（裸 INSERT 撞部分唯一
            // 索引会静默丢行，且关停 flush 无自愈）
            let result = (|| -> rusqlite::Result<i64> {
                let rowid = execute_event(&tx, e)?;
                if with_agg {
                    super::agg::apply_event(&tx, e, rowid)?;
                }
                Ok(rowid)
            })();
            match result {
                Ok(rowid) => {
                    if let Err(c) = tx.commit() {
                        log::warn!("单条提交失败: {c}");
                        note_write_failure(1);
                        rowids.push(0);
                    } else {
                        rowids.push(rowid);
                    }
                }
                Err(err) => {
                    log::warn!("单条事件写入失败（已跳过）: {err}");
                    note_write_failure(1);
                    rowids.push(0);
                }
            }
        }
        rowids
    }

    fn insert_events_one_by_one(&self, events: &[Event]) -> Vec<i64> {
        self.insert_one_by_one_impl(events, false)
    }

    fn insert_events_with_agg_one_by_one(&self, events: &[Event]) -> Vec<i64> {
        self.insert_one_by_one_impl(events, true)
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
                // agg 三表的 date 一律为**本地时区**日历日 "YYYY-MM-DD"（db/agg.rs
                // apply_event 用 Local.format("%Y-%m-%d")，update_agg 用
                // substr(datetime(timestamp, off),1,10)），故清理边界同样取 cutoff
                // 时刻对应的本地日历日。
                let cutoff_date = cutoff
                    .with_timezone(&chrono::Local)
                    .format("%Y-%m-%d")
                    .to_string();
                // 原始与聚合必须同事务一并清理：热力图/趋势只读 daily_agg，
                // 若只删 events 留下过期聚合行，聚合视图会展示已被清理的日期，
                // 且与基于 raw 的视图对不上。
                let deleted = (|| -> rusqlite::Result<usize> {
                    conn.execute_batch("BEGIN IMMEDIATE")?;
                    let deleted = conn.execute("DELETE FROM events WHERE timestamp < ?1", [&cutoff_str]);
                    let daily = conn.execute("DELETE FROM daily_agg WHERE date < ?1", [&cutoff_date]);
                    let minute = conn.execute("DELETE FROM agg_minute WHERE date < ?1", [&cutoff_date]);
                    let app = conn.execute(
                        "DELETE FROM agg_daily WHERE bucket_id LIKE 'app:%' AND date < ?1",
                        [&cutoff_date],
                    );
                    let deleted = (deleted?, daily?, minute?, app?);
                    conn.execute_batch("COMMIT")?;
                    Ok(deleted.0)
                })();
                match deleted {
                    Ok(deleted) => {
                        if deleted > 0 {
                            log::info!(
                                "清理了 {} 条超过 {} 天的旧事件（daily_agg/agg_minute/agg_daily 同步清理）",
                                deleted,
                                self.retention_days
                            );
                        }
                        deleted
                    }
                    Err(e) => {
                        // 回滚部分删除，避免原始/聚合清理到一半的不一致状态
                        let _ = conn.execute_batch("ROLLBACK");
                        log::error!("清理旧事件失败: {e}");
                        0
                    }
                }
            },
            || 0,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{EventAction, EventType};

    /// P0 注入测试：删除 events 表使一切写入必然失败（等效磁盘故障/只读），
    /// 断言 WRITE_FAILURES 计数增长且所有 rowid 落 0。
    #[test]
    fn write_failures_are_counted() {
        let dir = std::env::temp_dir().join(format!(
            "kyn-wf-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("wf.db");
        let db = Database::open(db_path.to_str().unwrap()).unwrap();
        db.with_writer(
            |c| {
                let _ = c.execute("DROP TABLE events", []);
            },
            || (),
        );

        let before = crate::collector::WRITE_FAILURES.load(std::sync::atomic::Ordering::Relaxed);
        let e = Event::new(EventAction::Press, EventType::Keyboard);
        let rowids = db.insert_events(&[e.clone(), e]);
        let after = crate::collector::WRITE_FAILURES.load(std::sync::atomic::Ordering::Relaxed);
        assert!(
            after > before,
            "写失败必须累加 WRITE_FAILURES（before={before}, after={after}）"
        );
        assert_eq!(rowids, vec![0, 0]);

        // 含聚合维护的主路径同样计数
        let before = after;
        let _ = db.insert_events_with_agg(&[Event::new(EventAction::Press, EventType::Keyboard)]);
        let after = crate::collector::WRITE_FAILURES.load(std::sync::atomic::Ordering::Relaxed);
        assert!(after > before, "with_agg 降级路径也必须计数");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 交叉审查 P4 契约：降级路径返回的 rowid Vec 必须与 events **一一对应**
    /// （失败行 push 0），任何失败路径都不得使返回值短于输入。
    #[test]
    fn one_by_one_rowids_len_always_matches_events() {
        let dir = std::env::temp_dir().join(format!(
            "kyn-wflen-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("wf.db");
        let db = Database::open(db_path.to_str().unwrap()).unwrap();

        // 正常路径：长度一一对应
        let events: Vec<Event> = (0..3)
            .map(|_| Event::new(EventAction::Press, EventType::Keyboard))
            .collect();
        assert_eq!(db.insert_events(&events).len(), 3);
        assert_eq!(db.insert_events_with_agg(&events).len(), 3);

        // 全失败路径（DROP events 表 → 单条执行必然失败）：仍一一对应且全 0
        db.with_writer(
            |c| {
                let _ = c.execute("DROP TABLE events", []);
            },
            || (),
        );
        let rowids = db.insert_events(&events);
        assert_eq!(rowids, vec![0, 0, 0], "失败行必须 push 0 保持一一对应");
        let rowids_agg = db.insert_events_with_agg(&events);
        assert_eq!(rowids_agg, vec![0, 0, 0]);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
