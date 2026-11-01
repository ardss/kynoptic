//! `events` 表的写入与清理
//!
//! 批量插入带多级降级（整批事务 → 子批事务（50 条/事务）→ 单条独立事务），
//! 确保成功行不丢失。子批化（WAL 写放大修复）：外部写者持锁竞争下整批失败
//! 后，逐条降级原本把 300 条批拆成 ~300 个独立事务（WAL 增长实测 ~15-20x）；
//! 每 50 条一个事务（~6 提交/批，实测 ~3x），子批内真正出现行级冲突才退回逐条。
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

/// input_agg 行的部分唯一索引键预查（migration 0005 ux_input_agg_minute）：
/// UPSERT **前**判定该键是否已存在——存在则本次走 DO UPDATE 覆盖（行数 +0），
/// 不存在则 INSERT 分支新插入（行数 +1，且 last_insert_rowid 此时有效——旧
/// 实现在 UPSERT **后**回查正是为了避开 DO UPDATE 路径的 last_insert_rowid
/// 残留，S1.2 起预查一并替代该回查：命中即得真实 id，未命中由 INSERT 分支的
/// last_insert_rowid 取新 id，两者皆精确）。O(log N) 索引命中，供水位增量
/// 判定与聚合维护 id 双用。
const INPUT_AGG_EXISTENCE_SQL: &str = "
SELECT id FROM events
WHERE timestamp = ?1 AND event_type = ?2 AND event_action = 'input_agg'
";

/// 写失败计数（P0，「批」口径）：磁盘写满/写失败时旧实现只 log::warn（且"降级逐条→跳过"
/// 后无任何聚合观测），告警看门狗只看通道满载计数，写失败完全静默。各整批失败点
/// （整批尝试失败、重试失败）每批只累加一次；降级逐条路径的**行级**失败数不进此
/// 计数（一批 ~300 行全败会让「批」口径虚高 ~300 倍，看门狗 60s 汇总与
/// collector-error.log 留档跟着失真）——行级数由 collector::WRITE_FAILED_ROWS
/// 独立观测（见 note_failed_rows），看门狗汇总时并列展示。
fn note_write_failure(n: u64) {
    crate::collector::WRITE_FAILURES.fetch_add(n, std::sync::atomic::Ordering::Relaxed);
}

/// 降级路径行级失败行计数（观测口径，独立于「批」口径的 WRITE_FAILURES，见上）。
fn note_failed_rows(n: u64) {
    crate::collector::note_failed_rows(n);
}

/// 降级逐条后判断"整批全败"（rowid 全 0 即无一成功落库），供整批兜底重试。
fn all_failed(rowids: &[i64], expected: usize) -> bool {
    rowids.len() == expected && rowids.iter().all(|&r| r == 0)
}

/// 时间戳规范化（审查 HIGH：格式漂移防护）：统一改写为 UTC `+00:00` 形的
/// RFC3339。events.timestamp 的全部范围谓词是字符串比较，一旦带非 +00:00
/// 后缀（采集器 bug 或外部导入写成 '+08:00'/'Z'），字典序不再等于时间序，
/// 事件会被静默计入错误的"今日"或直接丢弃。可解析的时间戳就地归一化；
/// 不可解析的保持原样（采集写入侧已在 reject_future_events 拒收此类事件；
/// 本函数兜底的是绕过 collector 直连 insert_events 的路径，读侧容错照旧）。
fn normalize_timestamp(ts: &str) -> String {
    match chrono::DateTime::parse_from_rfc3339(ts) {
        // to_rfc3339 输出固定 +00:00 后缀，字典序 == 时间序
        Ok(t) => t.with_timezone(&chrono::Utc).to_rfc3339(),
        Err(_) => ts.to_string(),
    }
}

/// 单条事件落库（input_agg 行走 UPSERT，其余 INSERT）。返回该行的 events
/// rowid（input_agg 聚合行无稳定新 rowid，返回 0，与旧 insert_events 语义
/// 一致——对外契约，见 [`execute_event_rowids`] 返回值 .0）。
/// 单条事件落库 + rowid 双语义拆分 + 新行判定（rowid/聚合/水位增量契约统一
/// 批）。返回 `(对外 rowid, agg 维护 rowid, 新增行数)`：
///
/// - **对外 rowid**（.0）：既有契约不变——input_agg 行恒 0。消费方锚点：
///   collector 失败行判据（「r==0 且 action!=InputAgg」才逐条重试）、
///   all_failed（全零即整批失败）、vec![0] 契约测试（all_failed_flag_
///   is_explicit_for_pure_input_agg_batch 等）。
/// - **agg 维护 rowid**（.1）：传给 `agg::apply_event` 的**真实** events.id，
///   供混合分钟探测与 max_event_rowid 守卫使用。input_agg 行按部分唯一索引
///   预查取 id（[`INPUT_AGG_EXISTENCE_SQL`]，S1.2 起预查在 UPSERT 前执行，
///   命中即得原行 id；未命中走 INSERT 分支后 last_insert_rowid 有效——
///   **禁用 UPSERT 后的 last_insert_rowid**：DO UPDATE 路径不更新该值，会
///   返回上一条 INSERT 路径的残留 id，喂给聚合维护即探测/守卫全错）。
///   raw 行两值相同。`with_agg_id=false` 时 input_agg 行 agg rowid 记 0
///   （`insert_events` 无聚合维护路径）。
/// - **新增行数**（.2，S1.2）：本行对 events 行数的净贡献——raw 行成功 +1、
///   失败不计；input_agg 行 INSERT 分支 +1、DO UPDATE 覆盖 +0。批量路径把
///   各行贡献求和喂给 [`super::bump_events_watermark`]（O(1) 增量水位，
///   替代每批 O(N) COUNT(*)），其口径一致性由
///   incremental_watermark_matches_count_over_random_insert_delete_sequence
///   钉住。
fn execute_event_rowids(
    tx: &Connection,
    e: &Event,
    with_agg_id: bool,
) -> rusqlite::Result<(i64, i64, i64)> {
    let data_str = e.event_data.as_ref().map(|v| v.to_string());
    // 审查 HIGH：写库统一规范化时间戳（见 normalize_timestamp），
    // 保证字符串范围谓词（timestamp >= ?1 AND < ?2）语义正确
    let ts = normalize_timestamp(&e.timestamp);
    // input_agg 聚合行走 UPSERT（同分钟同类型覆盖），其余照旧 INSERT
    let is_agg = e.event_action == crate::types::EventAction::InputAgg;
    if is_agg {
        // 预查（O(log N) 部分唯一索引）：既判新行供水位增量，又取真实 id
        let existed: Option<i64> = tx
            .query_row(
                INPUT_AGG_EXISTENCE_SQL,
                params![ts, e.event_type.as_str()],
                |r| r.get(0),
            )
            .ok();
        // prepare_cached 复用 prepared statement 计划，避免每行重新 prepare/finalize
        let mut stmt = tx.prepare_cached(UPSERT_INPUT_AGG_SQL)?;
        stmt.execute(params![
            ts,
            e.event_type.as_str(),
            e.event_action.as_str(),
            data_str,
            e.app_name,
            e.window_title,
            e.session_id,
        ])?;
        let agg_rowid = match existed {
            Some(id) => id, // DO UPDATE 覆盖：行数 +0，id 不变
            None => {
                // INSERT 分支：行数 +1；此时 last_insert_rowid 即新行 id
                if with_agg_id {
                    tx.last_insert_rowid()
                } else {
                    0
                }
            }
        };
        return Ok((0, agg_rowid, if existed.is_none() { 1 } else { 0 }));
    }
    let mut stmt = tx.prepare_cached(INSERT_SQL)?;
    stmt.execute(params![
        ts,
        e.event_type.as_str(),
        e.event_action.as_str(),
        data_str,
        e.app_name,
        e.window_title,
        e.session_id,
    ])?;
    let external = tx.last_insert_rowid();
    Ok((external, external, 1))
}

impl Database {
    /// 批量插入事件（只插 events，不做聚合维护）。
    ///
    /// 健壮性：单条失败不会让整批丢失——
    /// 1. 第一次尝试：事务内逐条 INSERT（整批一个事务）
    /// 2. 失败 → 降级为子批提交（每 50 条一个独立事务，WAL 写放大小一个量级）
    /// 3. 子批内出现行级冲突 → 该子批回滚后逐条独立事务重试（逐条语义不变：
    ///    成功行 rowid 照常返回，失败行记 0）
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
    /// 返回（rowids, 整批全失败布尔）：rowids 与 `events` 一一对应（同
    /// [`Database::insert_events`]）；布尔位**显式**区分「本批一条都没写进去」
    /// 与「input_agg 批合法的 rowid=0 成功」——此前调用方只能按全零 rowid 猜，
    /// 纯 input_agg 批整批失败会被误判成已落库（心跳时钟照常推进）。
    pub fn insert_events_with_agg(&self, events: &[Event]) -> (Vec<i64>, bool) {
        if events.is_empty() {
            return (Vec::new(), false);
        }

        match self.insert_events_with_agg_tx(events) {
            Ok(rowids) => (rowids, false),
            Err(first) => {
                // 审查 P1：外部写者（backfill/agg-heal 分块）可能持有写锁超过
                // writer 的 busy_timeout，整批事务 SQLITE_BUSY 失败并不代表数据
                // 有问题；这批事件已从通道 pop 出，降级逐条若同样全撞 busy 就
                // 是永久丢失（丢已 pop 的事件即数据丢失，非可丢弃错误）。先睡
                // 500ms 让外部写者的当前块写完，整批重试一次，仍失败才降级逐条。
                log::warn!("批量插入（含聚合维护）失败，500ms 后整批重试一次: {first}");
                std::thread::sleep(std::time::Duration::from_millis(500));
                match self.insert_events_with_agg_tx(events) {
                    Ok(rowids) => (rowids, false),
                    Err(e) => {
                        note_write_failure(1);
                        log::error!("批量插入（含聚合维护）重试仍失败，降级为子批提交: {e}");
                        let rowids = self.insert_events_with_agg_one_by_one(events);
                        // 审查 P1：降级路径也全败（busy 风暴未退，整批已 pop 出通道）
                        // → 最后再整批兜底重试一次，避免整块事件永久丢失。
                        // （纯 input_agg 批成功时 rowid 也全为 0，会多一次幂等
                        // UPSERT 重试，无害。）
                        if all_failed(&rowids, events.len()) {
                            std::thread::sleep(std::time::Duration::from_millis(500));
                            match self.insert_events_with_agg_tx(events) {
                                Ok(rowids2) => {
                                    log::info!(
                                        "子批降级全败后整批兜底重试成功（{} 条）",
                                        events.len()
                                    );
                                    return (rowids2, false);
                                }
                                Err(e2) => {
                                    log::error!(
                                        "整批兜底重试仍失败，{} 条事件全部未落库: {e2}",
                                        events.len()
                                    );
                                    // 整批全失败显式上抛：调用方（write_batch）据此
                                    // 不推进心跳时钟、不推进批次游标，并保留事件重试。
                                    return (rowids, true);
                                }
                            }
                        }
                        (rowids, false)
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
        // S1.2 增量水位：累计本批真实新增行数（raw 成功 +1；input_agg 预检
        // INSERT 分支 +1、DO UPDATE 覆盖 +0），事务内 O(1) 累加进水位——
        // 取代每批 SELECT COUNT(*) 的 O(N) 全索引扫描（写锁持有时间不再随
        // 库行数增长）。批回滚时累加随事务一并回滚。
        let mut new_rows = 0i64;
        for e in events {
            let (rowid, _, added) = execute_event_rowids(&tx, e, false)?;
            new_rows += added;
            rowids.push(rowid);
        }
        // 与事件写入同事务推进 events 水位（WAL 静默蒸发防护，见
        // db::persist_events_watermark；W33-F2 起水位为行数口径，S1.2 起
        // 批路径 O(1) 增量累加）：本批提交后水位即含本批，回滚时水位随之
        // 回滚，不会留下「水位超前数据」。
        super::bump_events_watermark(&tx, new_rows);
        tx.commit()?;
        Ok(rowids)
    }

    fn insert_events_with_agg_tx(&self, events: &[Event]) -> rusqlite::Result<Vec<i64>> {
        let Some(conn) = lock_writer(&self.writer, &self.db_path) else {
            return Err(rusqlite::Error::ExecuteReturnedResults);
        };
        let tx = conn.unchecked_transaction()?;
        let mut rowids: Vec<i64> = Vec::with_capacity(events.len());
        // S1.2 增量水位（同 insert_events_tx）：按行精确累计真实新增行数，
        // 事务内 O(1) 累加，取代每批 O(N) COUNT(*)（口径一致性见
        // incremental_watermark_matches_count_over_random_insert_delete_sequence）
        let mut new_rows = 0i64;
        for e in events {
            // rowid 双语义（混合分钟契约统一批）：对外返回值保持 input_agg→0
            // 契约，聚合维护拿真实 id（input_agg 行按部分唯一索引预查/INSERT
            // 分支 last_insert_rowid 取得）——此前传 0 使 detect_mixed_minute
            // 生产永不命中，混合分钟重算为死代码（快照 MAX 吞 raw 段计数，
            // agg 与 events SUM 分叉）。
            let (rowid, agg_rowid, added) = execute_event_rowids(&tx, e, true)?;
            new_rows += added;
            // 聚合增量与事件写入同事务：max_event_rowid 守卫保留作为幂等兜底
            // （事务化后"回填重算与增量并发"的交错窗口消失，重复投递仍被跳过）
            super::agg::apply_event(&tx, e, agg_rowid)?;
            rowids.push(rowid);
        }
        // 同事务推进 events 水位（WAL 静默蒸发防护，见 db::persist_events_
        // watermark；W33-F2 起行数口径，S1.2 起批路径 O(1) 增量）：events+
        // agg+水位同生共死，回滚时一并回滚。纯 input_agg 覆盖批无新行时
        // delta=0 跳过写；同分钟首写新增一行则计入。
        super::bump_events_watermark(&tx, new_rows);
        tx.commit()?;
        Ok(rowids)
    }

    /// 降级路径每个子批的事件数（WAL 写放大修复）：外部写者持写锁竞争时
    /// （后台回填/欠聚合自愈的 BEGIN IMMEDIATE 分块事务），纯逐条降级会把
    /// 300 条批拆成 ~300 个独立事务提交，WAL 增长实测 ~15-20x。子批 = 每 50
    /// 条一个事务（300 条约 6 次提交，实测 ~3x 放大）；子批内**真正**出现行级
    /// 冲突（单条 execute/commit 失败）才回滚退回逐条独立事务（子批回滚把已做
    /// 的 agg 更新一并回滚，逐条重放时 max_event_rowid 守卫与 input_agg 的
    /// MAX 覆盖语义保证不重复累计）。「rowid 与 events 一一对应、失败行 push
    /// 0」契约不变。
    const FALLBACK_CHUNK_SIZE: usize = 50;

    /// 降级路径公共体：子批提交（每 [`FALLBACK_CHUNK_SIZE`] 条一个独立事务），
    /// 子批级失败时退回逐条（可选附带聚合维护）。
    ///
    /// 契约：返回 Vec 与 `events` **一一对应**——失败的行 push 0（rowid 0 在
    /// agg 增量维护中被 max_event_rowid 守卫跳过，不会重复累计）。交叉审查
    /// P4 修复：此前 lock_writer / 事务创建失败的 continue 路径不 push，返回
    /// Vec 短于 events，违反 doc 契约（调用方按索引 zip 会错位）。
    /// 限流：禁止逐条 warn 洪水与逐条 WRITE_FAILURES 累加（「批」口径虚高
    /// ~300 倍）——批末一条汇总日志，行级计数进 collector::WRITE_FAILED_ROWS
    /// 观测计数器（见 note_write_failure 文档）。
    fn insert_one_by_one_impl(&self, events: &[Event], with_agg: bool) -> Vec<i64> {
        let mut rowids: Vec<i64> = Vec::with_capacity(events.len());
        let mut failed_rows = 0usize;
        let mut first_error: Option<String> = None;
        // S1.2 增量水位：只累计**已提交**子批/单条的真实新增行数，收尾一次
        // O(1) 累加（回滚的子批不计——与旧行为「收尾按 COUNT(*) 刷新」对
        // 已提交内容的口径等价）
        let mut committed_new_rows = 0i64;

        for chunk in events.chunks(Self::FALLBACK_CHUNK_SIZE) {
            match self.write_chunk_tx(chunk, with_agg) {
                Some((ids, added)) => {
                    committed_new_rows += added;
                    rowids.extend(ids);
                }
                // 子批级失败（写连接/事务创建失败，或行级冲突触发回滚）：退回
                // 逐条重试（保持旧「每条独立取锁」语义），短暂锁竞争不会整
                // 子批跳过。
                None => {
                    for e in chunk {
                        let (id, added) = self.write_row_degraded(
                            e,
                            with_agg,
                            &mut failed_rows,
                            &mut first_error,
                        );
                        committed_new_rows += added;
                        rowids.push(id);
                    }
                }
            }
        }

        // 批末限流（写失败计数口径）：一批一条汇总日志 + 一次行级观测计数。
        if failed_rows > 0 {
            note_failed_rows(failed_rows as u64);
            log::warn!(
                "{failed_rows}/{} 条单条写入失败（已跳过，记 0）: {}",
                events.len(),
                first_error.unwrap_or_default()
            );
        }

        // 降级路径收尾：整批处理完（无论逐条成败）统一推进一次 events 水位
        // （S1.2 起 O(1) 增量累加已提交新增行数），写连接不可用时 with_writer
        // 自动跳过。
        self.with_writer(
            |conn| {
                super::bump_events_watermark(conn, committed_new_rows);
            },
            || (),
        );
        rowids
    }

    /// 子批事务：单事务写一个子批（with_agg 时聚合维护同事务，与整批路径
    /// 同语义），全行成功才提交；任一行级冲突或提交失败即回滚返回 None
    /// （事务随 tx 丢弃而回滚），由调用方把该子批退回逐条。
    fn write_chunk_tx(&self, chunk: &[Event], with_agg: bool) -> Option<(Vec<i64>, i64)> {
        let conn = lock_writer(&self.writer, &self.db_path)?;
        let tx = conn.unchecked_transaction().ok()?;
        let mut ids = Vec::with_capacity(chunk.len());
        let mut new_rows = 0i64;
        for e in chunk {
            // rowid 双语义同整批路径（见 execute_event_rowids）：对外 push
            // input_agg→0 契约值，聚合维护拿回查后的真实 id；added 为本行
            // 对行数的净贡献（S1.2 增量水位）
            let (rowid, agg_rowid, added) = execute_event_rowids(&tx, e, with_agg).ok()?;
            new_rows += added;
            if with_agg {
                super::agg::apply_event(&tx, e, agg_rowid).ok()?;
            }
            ids.push(rowid);
        }
        tx.commit().ok()?;
        Some((ids, new_rows))
    }

    /// 逐条降级写（子批级失败后的兜底，即旧逐条路径主体）：独立取锁 + 独立
    /// 事务。失败记一行计数并返回 (0, 0)（rowid 0 被 agg 维护的 max_event_
    /// rowid 守卫跳过，不重复累计；S1.2 新增行数 0 同样不进水位增量）；只
    /// 保留首条错误供批末汇总日志——逐条日志/计数禁止（洪水 + 虚高，见
    /// insert_one_by_one_impl 限流说明）。返回 (对外 rowid, 新增行数)。
    fn write_row_degraded(
        &self,
        e: &Event,
        with_agg: bool,
        failed_rows: &mut usize,
        first_error: &mut Option<String>,
    ) -> (i64, i64) {
        let mut record_fail = |err: String| {
            *failed_rows += 1;
            if first_error.is_none() {
                *first_error = Some(err);
            }
        };
        let Some(conn) = lock_writer(&self.writer, &self.db_path) else {
            record_fail("写连接不可用".to_string());
            return (0, 0);
        };
        let Ok(tx) = conn.unchecked_transaction() else {
            record_fail("事务创建失败".to_string());
            return (0, 0);
        };
        // 降级路径 input_agg 行同样必须走 UPSERT（裸 INSERT 撞部分唯一
        // 索引会静默丢行，且关停 flush 无自愈）。rowid 双语义同整批路径
        // （见 execute_event_rowids）
        let result = (|| -> rusqlite::Result<(i64, i64)> {
            let (rowid, agg_rowid, added) = execute_event_rowids(&tx, e, with_agg)?;
            if with_agg {
                super::agg::apply_event(&tx, e, agg_rowid)?;
            }
            Ok((rowid, added))
        })();
        match result {
            Ok((rowid, added)) => match tx.commit() {
                Ok(()) => (rowid, added),
                Err(c) => {
                    record_fail(format!("单条提交失败: {c}"));
                    (0, 0)
                }
            },
            Err(err) => {
                record_fail(format!("单条事件写入失败: {err}"));
                (0, 0)
            }
        }
    }

    fn insert_events_one_by_one(&self, events: &[Event]) -> Vec<i64> {
        self.insert_one_by_one_impl(events, false)
    }

    fn insert_events_with_agg_one_by_one(&self, events: &[Event]) -> Vec<i64> {
        self.insert_one_by_one_impl(events, true)
    }

    /// 清理超过保留天数的事件（生产入口：真实时钟 → 注入式收口）。
    pub fn cleanup_old_events(&self) -> usize {
        self.cleanup_old_events_at(chrono::Utc::now().timestamp_millis())
    }

    /// 注入时钟版清理（`now_ms` = UTC epoch 毫秒）：测试零真实时钟依赖
    /// （红线「测试不得读真实时钟」），生产行为与 [`Self::cleanup_old_events`]
    /// 完全一致。
    pub fn cleanup_old_events_at(&self, now_ms: i64) -> usize {
        if self.retention_days <= 0 {
            return 0; // 0 = 永不删除（默认）。铁律：原始数据只能显式 opt-in 才清理。
        }
        self.with_writer(
            |conn| {
                let cutoff = chrono::DateTime::from_timestamp_millis(now_ms)
                    .unwrap_or_else(chrono::Utc::now)
                    - chrono::Duration::days(self.retention_days);
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
                    // S1.1：actor 证据缓存（0.4.0 F1）此前是清理盲区——agg_minute
                    // 被清而 agg_minute_actor 留下孤儿行，表只增不减。与下面
                    // agg_daily 同界同事务（不变量见测试
                    // raw_and_agg_minute_actor_cleanup_are_synchronized）。
                    let minute_actor = conn.execute(
                        "DELETE FROM agg_minute_actor WHERE date < ?1",
                        [&cutoff_date],
                    );
                    let app = conn.execute(
                        "DELETE FROM agg_daily WHERE bucket_id LIKE 'app:%' AND date < ?1",
                        [&cutoff_date],
                    );
                    // M2.4：净指标分钟桶表同事务清理（桶口径=UTC epoch 分钟，
                    // 与本函数 cutoff 同源换算；默认 retention=0 时整段不可达）
                    let net_minute = conn.execute(
                        "DELETE FROM net_process_minute WHERE minute_bucket < ?1",
                        [cutoff.timestamp_millis().div_euclid(60_000)],
                    );
                    // 水位自 W33-F2 起是行数口径（COUNT(*)）：显式 opt-in 保留期
                    // 清理删行后必须在**同一事务内（COMMIT 之前）**推进水位——
                    // 否则下次 open 的对账会把合法的「清理后减少」误判为数据
                    // 丢失（回退告警 + 降级旗标）。回归修复：此前水位推进在
                    // COMMIT 之后（两条独立自动提交语句），进程恰在删行提交与
                    // 水位 upsert 之间被强杀/断电时，持久化水位仍停在清理前
                    // 行数 → 下次 open 假报「回退」。移进同事务后删行与水位
                    // 推进随 COMMIT 原子落库，事务失败回滚时水位也一并还原。
                    let deleted =
                        (deleted?, daily?, minute?, minute_actor?, app?, net_minute?);
                    if deleted.0 > 0 {
                        super::persist_events_watermark(conn);
                    }
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

    /// 临时库目录（红线：测试不读真实时钟）：纯 pid+seq（AtomicUsize 计数），
    /// 上次运行遗留同名目录（panic 跳过清理/删库时连接未关闭）则递增 seq 取空名。
    fn tmp_dir(tag: &str) -> std::path::PathBuf {
        static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let mut seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        loop {
            let dir = std::env::temp_dir().join(format!("kyn-{tag}-{}-{seq}", std::process::id()));
            if !dir.exists() {
                return dir;
            }
            seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// P0 注入测试：删除 events 表使一切写入必然失败（等效磁盘故障/只读），
    /// 断言 WRITE_FAILURES 计数增长且所有 rowid 落 0。
    #[test]
    fn write_failures_are_counted() {
        let dir = tmp_dir("wf");
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
        let dir = tmp_dir("wflen");
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("wf.db");
        let db = Database::open(db_path.to_str().unwrap()).unwrap();

        // 正常路径：长度一一对应
        let events: Vec<Event> = (0..3)
            .map(|_| Event::new(EventAction::Press, EventType::Keyboard))
            .collect();
        assert_eq!(db.insert_events(&events).len(), 3);
        assert_eq!(db.insert_events_with_agg(&events).0.len(), 3);

        // 全失败路径（DROP events 表 → 单条执行必然失败）：仍一一对应且全 0
        db.with_writer(
            |c| {
                let _ = c.execute("DROP TABLE events", []);
            },
            || (),
        );
        let rowids = db.insert_events(&events);
        assert_eq!(rowids, vec![0, 0, 0], "失败行必须 push 0 保持一一对应");
        let (rowids_agg, agg_all_failed) = db.insert_events_with_agg(&events);
        assert_eq!(rowids_agg, vec![0, 0, 0]);
        assert!(agg_all_failed, "非空批整批失败必须显式报告 all_failed");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 结构性修复（写失败路径）：insert 层必须显式区分「纯 input_agg 批整批
    /// 写入失败」与「input_agg 批合法的 rowid=0 成功」——旧实现两者同为全零
    /// rowid，write_batch 误判已落库、心跳照常推进。
    #[test]
    fn all_failed_flag_is_explicit_for_pure_input_agg_batch() {
        let dir = tmp_dir("aggfail");
        std::fs::create_dir_all(&dir).unwrap();

        // 对照组：正常写纯 input_agg 批 → all_failed=false（rowid 合法为 0）
        let ok_path = dir.join("ok.db");
        let ok_db = Database::open(ok_path.to_str().unwrap()).unwrap();
        let e = Event::new(EventAction::InputAgg, EventType::Keyboard);
        let (rowids, all_failed) = ok_db.insert_events_with_agg(std::slice::from_ref(&e));
        assert!(!all_failed, "成功的纯 input_agg 批不得报告全失败");
        assert_eq!(rowids, vec![0]);

        // 故障组：DROP events 表（等效磁盘故障）→ 纯 input_agg 批全败必须显式上抛
        let bad_path = dir.join("bad.db");
        let bad_db = Database::open(bad_path.to_str().unwrap()).unwrap();
        bad_db.with_writer(
            |c| {
                let _ = c.execute("DROP TABLE events", []);
            },
            || (),
        );
        let (rowids, all_failed) = bad_db.insert_events_with_agg(&[e.clone(), e]);
        assert!(all_failed, "纯 input_agg 批整批失败必须显式返回 true");
        assert_eq!(rowids, vec![0, 0]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 结构性修复（WAL 静默蒸发防护 2026-09；W33-F2 起水位改行数口径，
    /// S1.2 起批路径按行增量累加）：每次批量事件落库后把 events 水位与写入
    /// 同事务推进持久化——
    /// 此前水位只在正常停机写一次，「停机后写入再丢失」整段不可检测。
    /// 此测试锚定新行为：批量提交后 metadata 水位即等于当前 COUNT(*)，
    /// input_agg 批首写新增一行时行数随之推进。
    #[test]
    fn batch_write_persists_events_watermark() {
        let dir = tmp_dir("wm");
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("wm.db");
        let db = Database::open(db_path.to_str().unwrap()).unwrap();

        // 新库 open 时 reconcile 已把水位播种为 0（对账刷新为当前 COUNT(*)）
        assert_eq!(
            db.get_metadata("events_watermark_row_count"),
            Some("0".to_string()),
            "新库开库对账后水位应为 0"
        );

        let ev = Event::new(EventAction::Press, EventType::Keyboard);
        let events = vec![ev.clone(), ev.clone(), ev];
        let (rowids, all_failed) = db.insert_events_with_agg(&events);
        assert!(!all_failed, "正常批量写不得报全败");
        assert_eq!(rowids, vec![1, 2, 3], "3 条普通事件 rowid 应为 1..3");

        // 批量提交后水位推进到当前 COUNT(*)=3
        let wm: i64 = db
            .get_metadata("events_watermark_row_count")
            .expect("批量落库后必须已写入水位")
            .parse()
            .unwrap();
        let count: i64 = db
            .reader()
            .query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))
            .unwrap();
        assert_eq!(wm, 3, "水位应等于当前行数 COUNT(*)");
        assert_eq!(wm, count, "水位与 COUNT(*) 必须一致");

        // 纯 input_agg 批：同分钟同类型首写即插入新行（部分唯一索引只约束
        // 同键覆盖），COUNT(*) 推进、水位随之刷新（调用方拿到的 rowid 记 0
        // 只表示"无稳定新 rowid"，不代表表无新行）。
        let agg_ev = Event::new(EventAction::InputAgg, EventType::Keyboard);
        let (agg_rowids, agg_failed) = db.insert_events_with_agg(std::slice::from_ref(&agg_ev));
        assert!(!agg_failed, "纯 input_agg 批合法成功");
        assert_eq!(
            agg_rowids,
            vec![0],
            "input_agg 行调用方记 0（无稳定新 rowid）"
        );
        let wm2: i64 = db
            .get_metadata("events_watermark_row_count")
            .unwrap()
            .parse()
            .unwrap();
        let count2: i64 = db
            .reader()
            .query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            wm2, count2,
            "水位必须始终跟随当前 COUNT(*)（input_agg 新行也计入）"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 子批降级 + 批/行口径分离回归（WAL 写放大修复，2026-09）：
    /// 1) 正常库 300 条走子批提交（每 50 条一个事务），rowid 仍与输入 1:1；
    /// 2) DROP TABLE 强制整批失败 → 降级路径：rowids 全 0（一一对应契约），
    ///    WRITE_FAILURES 只按「批」口径增长（不再随 300 行虚高），行级失败
    ///    数完整记入 WRITE_FAILED_ROWS 观测计数器。
    #[test]
    fn subbatch_degraded_keeps_rowid_contract_and_row_caliber() {
        // 对照组：正常库 300 条子批路径 rowid 一一对应
        let ok_dir = tmp_dir("subbatch");
        std::fs::create_dir_all(&ok_dir).unwrap();
        let ok_db = Database::open(ok_dir.join("ok.db").to_str().unwrap()).unwrap();
        let events: Vec<Event> = (0..300)
            .map(|_| Event::new(EventAction::Press, EventType::Keyboard))
            .collect();
        let rowids = ok_db.insert_events(&events);
        assert_eq!(
            rowids,
            (1..=300i64).collect::<Vec<i64>>(),
            "子批提交 rowid 必须一一对应且连续"
        );
        drop(ok_db);

        // 故障组：DROP events 表 → 整批失败 → 子批降级全败
        let bad_dir = tmp_dir("subbatch");
        std::fs::create_dir_all(&bad_dir).unwrap();
        let bad_db = Database::open(bad_dir.join("bad.db").to_str().unwrap()).unwrap();
        bad_db.with_writer(
            |c| {
                let _ = c.execute("DROP TABLE events", []);
            },
            || (),
        );
        let wf_before = crate::collector::WRITE_FAILURES.load(std::sync::atomic::Ordering::Relaxed);
        let rows_before =
            crate::collector::WRITE_FAILED_ROWS.load(std::sync::atomic::Ordering::Relaxed);
        let rowids = bad_db.insert_events(&events);
        let wf =
            crate::collector::WRITE_FAILURES.load(std::sync::atomic::Ordering::Relaxed) - wf_before;
        let rows = crate::collector::WRITE_FAILED_ROWS.load(std::sync::atomic::Ordering::Relaxed)
            - rows_before;
        assert_eq!(rowids, vec![0i64; 300], "失败行必须 push 0 保持一一对应");
        // 同进程其他写失败测试可能并发累加（只增不减）：本测试自身贡献
        // rows=300 / wf=1，故下限 300、上限取「批」口径远小于行数即可证。
        assert!(
            rows >= 300,
            "行级失败数必须记入观测计数器（本测试贡献 300 行，实测增量 {rows}）"
        );
        assert!(
            wf < 50,
            "WRITE_FAILURES 应保持「批」口径（增量 {wf}，不得随行数放大）"
        );

        let _ = std::fs::remove_dir_all(&ok_dir);
        let _ = std::fs::remove_dir_all(&bad_dir);
    }

    /// agg_minute + agg_minute_actor 全行指纹（含 max_event_rowid），供
    /// 「增量与重建逐行一致」的 parity 断言。
    fn agg_fingerprint(db: &Database) -> String {
        let reader = db.reader();
        let mut out = String::new();
        for sql in [
            "SELECT date||'|'||hour||'|'||minute||'|'||bucket_id||'|'\
             ||COALESCE(sum_value,0)||'|'||COALESCE(count_value,0)||'|'\
             ||COALESCE(max_event_rowid,0) \
             FROM agg_minute ORDER BY date, hour, minute, bucket_id",
            "SELECT date||'|'||hour||'|'||minute||'|'\
             ||COALESCE(keys_total,0)||'|'||COALESCE(keys_injected,0)||'|'\
             ||COALESCE(clicks_total,0)||'|'||COALESCE(clicks_injected,0)||'|'\
             ||COALESCE(scroll_total,0)||'|'||COALESCE(scroll_injected,0)||'|'\
             ||COALESCE(max_event_rowid,0) \
             FROM agg_minute_actor ORDER BY date, hour, minute",
        ] {
            let mut stmt = reader.prepare(sql).unwrap();
            let rows = stmt.query_map([], |r| r.get::<_, String>(0)).unwrap();
            for row in rows.flatten() {
                out.push_str(&row);
                out.push(';');
            }
        }
        out
    }

    /// S1.1 测试注入：retention_days 是 db 模块私有字段（生产默认 0 = 永不
    /// 删除），同模块测试可直接写值；不新增公开 setter（避免生产 API 面
    /// 扩大）。
    fn set_retention_days(db: &mut Database, days: i64) {
        db.retention_days = days;
    }

    /// 造两条跨保留期边界的事件（raw + with_agg 同事务，agg_minute 与
    /// agg_minute_actor 各两行）：`old_ts` 在保留期外，`recent_ts` 在保留期内。
    /// 时间全部相对 now 计算（过期/存留分界由 60d vs 30d vs 1h 的余量保证，
    /// 不依赖真实时钟的绝对值）。
    fn seed_across_retention_boundary(db: &Database) {
        let fmt =
            |t: chrono::DateTime<chrono::Utc>| t.format("%Y-%m-%dT%H:%M:%S+00:00").to_string();
        let old = fmt(chrono::Utc::now() - chrono::Duration::days(60));
        let recent = fmt(chrono::Utc::now() - chrono::Duration::hours(1));
        for ts in [old, recent] {
            let mut e = Event::new(EventAction::Press, EventType::Keyboard);
            e.timestamp = ts;
            let (rowids, failed) = db.insert_events_with_agg(std::slice::from_ref(&e));
            assert!(!failed, "正常写入不得失败");
            assert!(
                rowids.iter().any(|&r| r != 0),
                "raw 行必须有真实 rowid（写入成功前置）"
            );
        }
    }

    fn count(db: &Database, sql: &str) -> i64 {
        db.reader().query_row(sql, [], |r| r.get(0)).unwrap()
    }

    /// S1.1：设置 retention 后，过期的 agg_minute_actor 行必须被一并清理，
    /// 保留期内的 actor 行不得误删（actor 证据缓存此前是清理盲区——
    /// 删了 events/agg_minute 留下 actor 孤儿行，表只增不减）。
    #[test]
    fn retention_cleanup_clears_expired_agg_minute_actor_rows() {
        let dir = tmp_dir("s11actor");
        std::fs::create_dir_all(&dir).unwrap();
        let mut db = Database::open(dir.join("s11actor.db").to_str().unwrap()).unwrap();
        seed_across_retention_boundary(&db);
        assert_eq!(
            count(&db, "SELECT COUNT(*) FROM agg_minute_actor"),
            2,
            "前置：新旧两个日历日各一条 actor 行"
        );

        set_retention_days(&mut db, 30);
        let deleted = db.cleanup_old_events();
        assert_eq!(deleted, 1, "只应删除保留期外的原始事件（60 天前那条）");

        // 过期 actor 行被清、存留 actor 行保留
        let remaining: i64 = count(&db, "SELECT COUNT(*) FROM agg_minute_actor");
        assert_eq!(remaining, 1, "过期 actor 行必须被清理，存留行不得误删");

        // 与生产清理同口径的边界（本地日历日字符串比较）：残留 actor 行的
        // date 必须全部 >= cutoff_date
        let cutoff_date = (chrono::Utc::now() - chrono::Duration::days(30))
            .with_timezone(&chrono::Local)
            .format("%Y-%m-%d")
            .to_string();
        let reader = db.reader();
        let mut stmt = reader
            .prepare("SELECT date FROM agg_minute_actor ORDER BY date")
            .unwrap();
        let dates: Vec<String> = stmt
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert!(!dates.is_empty());
        for d in &dates {
            assert!(
                d.as_str() >= cutoff_date.as_str(),
                "残留 actor 行日期 {d} 必须不早于清理边界 {cutoff_date}"
            );
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// events.rs「原始与聚合必须同事务一并清理」不变量测试化（S1.1 扩展到
    /// agg_minute_actor）：清理后 actor 表的日期覆盖必须与存活原始事件的
    /// 本地日历日覆盖**完全一致**——不留孤儿 actor 行（展示已清理日期），
    /// 也不误删仍在保留期内的 actor 行（与 raw 视图对不上）。
    #[test]
    fn raw_and_agg_minute_actor_cleanup_are_synchronized() {
        let dir = tmp_dir("s11sync");
        std::fs::create_dir_all(&dir).unwrap();
        let mut db = Database::open(dir.join("s11sync.db").to_str().unwrap()).unwrap();

        // 三档日期：60d（过期删）、10d（保留期内留）、当天（留）
        let fmt =
            |t: chrono::DateTime<chrono::Utc>| t.format("%Y-%m-%dT%H:%M:%S+00:00").to_string();
        for offset in [-60i64, -10, 0] {
            let ts = fmt(chrono::Utc::now()
                + chrono::Duration::days(offset)
                + chrono::Duration::hours(12));
            let mut e = Event::new(EventAction::Press, EventType::Keyboard);
            e.timestamp = ts;
            let (rowids, failed) = db.insert_events_with_agg(std::slice::from_ref(&e));
            assert!(!failed);
            assert!(rowids.iter().any(|&r| r != 0));
        }

        set_retention_days(&mut db, 30);
        let deleted = db.cleanup_old_events();
        assert_eq!(deleted, 1, "只有 60 天前那条过期");

        // 存活原始事件的本地日历日集合
        let reader = db.reader();
        let mut stmt = reader.prepare("SELECT timestamp FROM events").unwrap();
        let surviving_days: Vec<String> = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .collect::<Result<Vec<String>, rusqlite::Error>>()
            .unwrap()
            .iter()
            .map(|ts| {
                chrono::DateTime::parse_from_rfc3339(ts)
                    .unwrap()
                    .with_timezone(&chrono::Local)
                    .format("%Y-%m-%d")
                    .to_string()
            })
            .collect();
        assert_eq!(surviving_days.len(), 2, "应剩 10 天前 + 当天两行");

        // actor 表日期集合
        let mut stmt = reader
            .prepare("SELECT DISTINCT date FROM agg_minute_actor ORDER BY date")
            .unwrap();
        let actor_days: Vec<String> = stmt
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();

        let mut raw_set = surviving_days.clone();
        raw_set.sort();
        raw_set.dedup();
        assert_eq!(
            actor_days, raw_set,
            "actor 日期覆盖必须与存活原始事件的日历日覆盖一致（无孤儿行、无误删行）"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 混合分钟 parity 走生产入口（探针A 序列，rowid/聚合契约统一批）。
    /// collector 真实 flush 形态：flush1 raw press ×5，flush2/flush3 同分钟
    /// input_agg 累计快照 keys=2→4（时间戳取分钟起点，命中部分唯一索引
    /// UPSERT 同一行）。此前生产写链 execute_event 对 input_agg 行恒返 0，
    /// apply_event 拿 rowid=0 → detect_mixed_minute 永不命中（首卫
    /// rowid<=0 早退）→ 快照 MAX 覆盖吞掉 raw 段计数（agg=MAX(5,4)=5 vs
    /// events SUM=9），混合分钟重算生产为死代码。本测试钉住：
    /// ① 对外 rowid 契约不变（input_agg 行恒 [0]，既有锚点口径）；
    /// ② agg_minute 增量结果与 events SUM 直读（消费面口径）相等；
    /// ③ agg_minute 与 agg_minute_actor 增量结果与 rebuild 逐行一致
    ///    （含 max_event_rowid 与 count 列）。
    /// 日期注入（禁真实时钟）：固定 2026-06-15T02:30 UTC 整分钟。
    #[test]
    fn contractpin_a_mixed_minute_parity_via_insert_events_with_agg() {
        let dir = tmp_dir("probea");
        std::fs::create_dir_all(&dir).unwrap();
        let db = Database::open(dir.join("probea.db").to_str().unwrap()).unwrap();

        // flush1：前半分钟 5 条 raw press（一批，raw 行对外 rowid 为真实 id）
        const MIN: &str = "2026-06-15T02:30";
        let mut flush1: Vec<Event> = Vec::new();
        for sec in [10u32, 15, 20, 25, 30] {
            let mut e = Event::new(EventAction::Press, EventType::Keyboard);
            e.timestamp = format!("{MIN}:{sec:02}+00:00");
            flush1.push(e);
        }
        let (rowids1, failed1) = db.insert_events_with_agg(&flush1);
        assert!(!failed1);
        assert_eq!(rowids1, vec![1, 2, 3, 4, 5], "raw 行对外 rowid 为真实 id");

        // flush2/flush3：采集器中途切计数模式，同分钟 input_agg 累计快照
        // keys=2 → keys=4（同分钟同类型 UPSERT 覆盖同一行；生产形态时间戳
        // 取分钟起点）。第二次写走 UPSERT 的 DO UPDATE 路径。
        let mk_agg = |keys: i64| {
            let mut e = Event::new(EventAction::InputAgg, EventType::Keyboard)
                .data(serde_json::json!({ "keys": keys, "samples": keys }))
                .app("", "");
            e.timestamp = format!("{MIN}:00+00:00");
            e
        };
        let (rowids2, failed2) = db.insert_events_with_agg(&[mk_agg(2)]);
        assert!(!failed2);
        assert_eq!(rowids2, vec![0], "对外契约：input_agg 行恒 0（不得改动）");
        let (rowids3, failed3) = db.insert_events_with_agg(&[mk_agg(4)]);
        assert!(!failed3);
        assert_eq!(rowids3, vec![0], "对外契约：input_agg 行恒 0（不得改动）");

        // 消费面口径：agg_minute 增量结果必须等于 events SUM 直读
        // （统一口径 = 事件 SUM：5 raw + input_agg 终值快照 keys=4 = 9；
        // 快照 MAX 覆盖给 MAX(5,4)=5，即探针A 的 5 vs 9 分叉）
        let reader = db.reader();
        let events_keys: i64 = reader
            .query_row(
                &format!(
                    "SELECT CAST(COALESCE(SUM({}),0) AS INTEGER) FROM events \
                     WHERE event_type IN ('keyboard','mouse','window')",
                    crate::queries::KEYS_ROW_EXPR
                ),
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(events_keys, 9, "5 条 raw press + input_agg 终值 keys=4");
        let incremental_keys: i64 = reader
            .query_row(
                "SELECT CAST(COALESCE(SUM(sum_value),0) AS INTEGER) FROM agg_minute \
                 WHERE bucket_id='input_keys'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            incremental_keys, events_keys,
            "混合分钟增量必须与 events SUM 直读（消费面）一致"
        );

        // parity：增量指纹与 rebuild 逐行一致（两套聚合缓存全行，含
        // max_event_rowid）
        let incremental_fp = agg_fingerprint(&db);
        db.rebuild_agg();
        assert_eq!(
            incremental_fp,
            agg_fingerprint(&db),
            "混合分钟（生产入口写序）增量与全量重建必须逐行一致"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
