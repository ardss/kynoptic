//! 聚合读缓存（agg_minute / agg_daily）——派生数据，原始 events 永不改动
//!
//! **定位（重要契约）**：本模块维护的 agg_minute / agg_daily 行是**纯派生的
//! 只读缓存**，与全量 SQL GROUP BY 现算结果等价。原始 events 表只增不改不删
//! （保留策略除外，与本模块无关）；聚合行随时可用 [`rebuild_all`] 从 events
//! 完整重建。get_anomalies 等全历史扫描查询读缓存提速，原始数据保持完全可查。
//!
//! bucket 语义（agg_minute.date/hour/minute 为**本地时区**分钟桶）：
//! - `input_keys`    sum=按键数, count=原始输入样本数
//! - `input_clicks`  sum=点击数
//! - `input_moves`   sum=移动距离(px), count=采样移动次数
//! - `window_switches` sum=count=窗口切换次数
//! - `input_keys_injected` / `input_clicks_injected`  sum=注入（自动化）计数
//!   （审查修复 2026-09：供深夜活动/APM 突增等异常口径扣减注入、只算人侧；
//!   旧缓存无该桶时读取方按 0 注入处理，等价旧行为）
//!
//! agg_daily 只存 `app:<name>` 行——**纯计数桶**（语义定案 2026-09）：
//! count=该应用当日事件数，sum_value 恒 NULL 且无读取方（见
//! [`UPSERT_DAILY_APP`] 注释），供 new_app_surge
//! 的全历史 per-app 统计从 O(全表) 降到 O(聚合行数)。
//!
//! **samples 语义（perf3 2026-09 对齐）**：agg_minute 的 count_value（samples）
//! = raw press + raw click + input_agg 行的 $.samples——**不含** raw move/scroll
//! 等非输入计数行。增量维护 [`apply_event`] 与全量重建 [`rebuild_all`] 必须对
//! 该语义逐行一致（perf3-longrun 一致性指纹曾抓到二者在含 raw move 的分钟上
//! 分叉：rebuild 把 move 计入 samples 而增量不会；count_value 当前无查询消费方，
//! 取「以增量语义为准」对齐）。
//!
//! 维护路径：
//! 1. 增量：writer 每次 flush 时与 events 落库**同一事务**调用 [`apply_event`]
//!    （见 `db::events::insert_events_with_agg`；审查 P1 事务化，kill 不再留下
//!    "events 有 agg 无"的欠聚合）；[`super::Database::update_agg`] 保留为
//!    独立维护入口（测试/兼容路径）。
//! 2. 懒回填：[`Database::open`] 时若 agg 全空而 events 非空 → [`backfill_if_needed`]
//!    （存量库首开自动补齐；选此方案而非 CLI 命令，见 CODE_NOTES.md §8）。
//! 3. 欠聚合自愈：[`under_agg_dates`] 廉价核对（events 中应有聚合贡献的原始行
//!    最大 id 与该日 agg_minute 已记录 max_event_rowid 对比），不一致日期由
//!    [`Database::open`] 后台与每日 maintenance 调 [`heal_under_agg`] 重算——
//!    兜底覆盖事务化之前遗留的欠聚合存量数据。

use chrono::{Local, Timelike, Utc};
use rusqlite::{params, Connection};

use crate::types::{Event, EventAction, EventType};

/// 本地分钟桶聚合行的写入（增量与重建共用）。
///
/// max_event_rowid 幂等防护（审查 P2）：每行记录已累计到的最大 events.id。
/// 增量写入带该事件的 rowid，rowid <= 已记录值的重复投递（如 backfill_chunk
/// 重算已包含该事件后的迟到增量）被 WHERE 守卫整体跳过，同事件不重复累计。
/// 重建/回填路径写入桶内 MAX(events.id)，保证重算后的迟到增量也能被识别。
const UPSERT_MINUTE: &str = "
INSERT INTO agg_minute (date, hour, minute, bucket_id, sum_value, count_value, max_event_rowid)
VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
ON CONFLICT(date, hour, minute, bucket_id) DO UPDATE SET
    sum_value = COALESCE(sum_value, 0) + excluded.sum_value,
    count_value = COALESCE(count_value, 0) + excluded.count_value,
    max_event_rowid = excluded.max_event_rowid
    WHERE excluded.max_event_rowid > COALESCE(agg_minute.max_event_rowid, 0)
";

/// input_agg 专用：事件携带的是"本分钟累计快照"，纯形态分钟同桶覆盖而非累加。
/// MAX 覆盖天然幂等（同 events 行原地 UPSERT，rowid 不变、值单调不减），
/// max_event_rowid 仅作单调推进，不需要守卫。
/// 混合分钟例外（同本地分钟已有先到 raw 行）：快照只覆盖本段、不构成完整
/// 分钟，不走本语句——改由 recompute_minute_buckets 以事件 SUM 统一口径重算。
const UPSERT_MINUTE_SNAPSHOT: &str = "
INSERT INTO agg_minute (date, hour, minute, bucket_id, sum_value, count_value, max_event_rowid)
VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
ON CONFLICT(date, hour, minute, bucket_id) DO UPDATE SET
    sum_value = MAX(COALESCE(sum_value, 0), excluded.sum_value),
    count_value = MAX(COALESCE(count_value, 0), excluded.count_value),
    max_event_rowid = MAX(COALESCE(agg_minute.max_event_rowid, 0), excluded.max_event_rowid)
";

// `app:<name>` 是**纯计数桶**（语义定案 2026-09）：count_value = 该应用当日
// 事件数，sum_value 恒为 NULL 且无读取方——消费方必须只按 count_value 求和，
// 禁止按 sum 列聚合（按 sum 求和会静默得 0/NULL）。agg_minute 各桶则
// sum/count 皆有语义（见模块头 bucket 语义）。
const UPSERT_DAILY_APP: &str = "
INSERT INTO agg_daily (date, bucket_id, sum_value, count_value)
VALUES (?1, 'app:' || ?2, NULL, ?3)
ON CONFLICT(date, bucket_id) DO UPDATE SET
    count_value = COALESCE(count_value, 0) + excluded.count_value
";

// ─── actor 证据缓存（0.4.0 F1，migration 0011）───────────────────────────────
// 契约（GPT 0.4.0 勘误 3）：agg_minute_actor 只存**单条输入事件可推导**的
// 分钟级证据计数；actor/channel/前台覆盖标签在查询层计算（queries/actor.rs），
// 绝不作为累加桶落库。合并语义与 agg_minute 对应桶逐条一致：
// - input_agg 行 → 快照 MAX 合并（UPSERT_ACTOR_SNAPSHOT，同 UPSERT_MINUTE_SNAPSHOT）
// - raw press/click 行 → 守卫累加（UPSERT_ACTOR_ADD，同 UPSERT_MINUTE 的
//   max_event_rowid WHERE 守卫：迟到重复投递整体跳过）
// - 混合分钟（同分钟已有先到 raw 行）→ 不走快照合并，recompute_minute_actor
//   以重建同款 SUM 重算
// 一个 apply_event 事件只触达自己模态的列；其余列传 0（MAX/累加均无副作用）。
const UPSERT_ACTOR_SNAPSHOT: &str = "
INSERT INTO agg_minute_actor (date, hour, minute, keys_total, keys_injected,
                              clicks_total, clicks_injected, scroll_total, scroll_injected,
                              max_event_rowid)
VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
ON CONFLICT(date, hour, minute) DO UPDATE SET
    keys_total      = MAX(COALESCE(keys_total, 0), excluded.keys_total),
    keys_injected   = MAX(COALESCE(keys_injected, 0), excluded.keys_injected),
    clicks_total    = MAX(COALESCE(clicks_total, 0), excluded.clicks_total),
    clicks_injected = MAX(COALESCE(clicks_injected, 0), excluded.clicks_injected),
    scroll_total    = MAX(COALESCE(scroll_total, 0), excluded.scroll_total),
    scroll_injected = MAX(COALESCE(scroll_injected, 0), excluded.scroll_injected),
    max_event_rowid = MAX(COALESCE(max_event_rowid, 0), excluded.max_event_rowid)
";

const UPSERT_ACTOR_ADD: &str = "
INSERT INTO agg_minute_actor (date, hour, minute, keys_total, keys_injected,
                              clicks_total, clicks_injected, scroll_total, scroll_injected,
                              max_event_rowid)
VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
ON CONFLICT(date, hour, minute) DO UPDATE SET
    keys_total      = COALESCE(keys_total, 0) + excluded.keys_total,
    keys_injected   = COALESCE(keys_injected, 0) + excluded.keys_injected,
    clicks_total    = COALESCE(clicks_total, 0) + excluded.clicks_total,
    clicks_injected = COALESCE(clicks_injected, 0) + excluded.clicks_injected,
    scroll_total    = COALESCE(scroll_total, 0) + excluded.scroll_total,
    scroll_injected = COALESCE(scroll_injected, 0) + excluded.scroll_injected,
    max_event_rowid = excluded.max_event_rowid
    WHERE excluded.max_event_rowid > COALESCE(agg_minute_actor.max_event_rowid, 0)
";

// ─── 分钟桶 count/辅助列表达式（全量重建 rebuild_all_inner、分块回填
// backfill_chunk、混合分钟重算 recompute_minute_buckets 三处共用，禁各自
// 内联漂移）────────────────────────────────────────────
// 口径约束（与增量维护 apply_event 逐条语义一一对应）：
// - raw press/click 按行 +1，input_agg 按 $.samples
// - 注入 count 勿用 ckeys/cclicks 全量充当：raw 注入按行 +1、input_agg 注入
//   按 $.samples（注入桶 count 与 sum 分口径）
// - moves 桶 sum 是 $.move_distance_px、count 是 $.moves；window_switches
//   仅 raw switch 行
const CNT_DIST_EXPR: &str =
    "SUM(CASE WHEN event_type='mouse' AND event_action='input_agg' AND json_valid(event_data) \
         THEN COALESCE(json_extract(event_data, '$.move_distance_px'), 0) ELSE 0 END)";
const CNT_SAMPLES_KEYS_EXPR: &str =
    "SUM(CASE WHEN event_type='keyboard' AND event_action='press' THEN 1 \
         WHEN event_type='keyboard' AND event_action='input_agg' AND json_valid(event_data) \
         THEN COALESCE(json_extract(event_data, '$.samples'), 0) ELSE 0 END)";
const CNT_SAMPLES_CLICKS_EXPR: &str =
    "SUM(CASE WHEN event_type='mouse' AND event_action='click' THEN 1 \
         WHEN event_type='mouse' AND event_action='input_agg' AND json_valid(event_data) \
         THEN COALESCE(json_extract(event_data, '$.samples'), 0) ELSE 0 END)";
const CNT_SAMPLES_INJ_KEYS_EXPR: &str =
    "SUM(CASE WHEN event_type='keyboard' AND event_action='press' \
         AND COALESCE(json_extract(event_data, '$.injected'), 0) != 0 THEN 1 \
         WHEN event_type='keyboard' AND event_action='input_agg' AND json_valid(event_data) \
         AND COALESCE(json_extract(event_data, '$.injected_keys'), 0) != 0 \
         THEN COALESCE(json_extract(event_data, '$.samples'), 0) ELSE 0 END)";
const CNT_SAMPLES_INJ_CLICKS_EXPR: &str =
    "SUM(CASE WHEN event_type='mouse' AND event_action='click' \
         AND COALESCE(json_extract(event_data, '$.injected'), 0) != 0 THEN 1 \
         WHEN event_type='mouse' AND event_action='input_agg' AND json_valid(event_data) \
         AND COALESCE(json_extract(event_data, '$.injected_clicks'), 0) != 0 \
         THEN COALESCE(json_extract(event_data, '$.samples'), 0) ELSE 0 END)";
const CNT_MOVES_EXPR: &str =
    "SUM(CASE WHEN event_type='mouse' AND event_action='input_agg' AND json_valid(event_data) \
         THEN COALESCE(json_extract(event_data, '$.moves'), 0) ELSE 0 END)";
const CNT_SWITCHES_EXPR: &str =
    "SUM(CASE WHEN event_type='window' AND event_action='switch' THEN 1 ELSE 0 END)";

/// 单个事件对聚合缓存的增量贡献（本地时区分钟桶）。
///
/// `rowid` = 该事件在 events 表的 rowid（落库时分配；未知传 0 → 增量行被
/// 守卫跳过，快照行仅更新计数快照）。
pub(crate) fn apply_event(conn: &Connection, e: &Event, rowid: i64) -> rusqlite::Result<()> {
    let ts = match chrono::DateTime::parse_from_rfc3339(&e.timestamp) {
        Ok(t) => t,
        Err(_) => return Ok(()), // 时间戳不可解析：跳过（不阻塞整批）
    };
    let local = ts.with_timezone(&chrono::Local);
    let date = local.format("%Y-%m-%d").to_string();
    let hour = local.hour() as i64;
    let minute = local.minute() as i64;

    // ─── 混合分钟探测（约束见 detect_mixed_minute）───
    let agg_minute_bounds = detect_mixed_minute(conn, e, &ts, rowid);
    // Some ⇔ 混合分钟命中（携带 UTC 分钟界供重算谓词）
    let agg_raw_mixed = agg_minute_bounds.is_some();

    match (e.event_type, e.event_action) {
        (EventType::Keyboard, EventAction::InputAgg) => {
            let keys = json_counter(e, "keys");
            // 键盘专属样本数优先（input_agg.rs 新增字段）；旧行回退 samples
            let ks = json_counter(e, "keys_samples");
            let samples = if ks > 0 {
                ks
            } else {
                json_counter(e, "samples")
            };
            // 注入（自动化）拆桶：与 input_keys 同一快照语义（审查修复 2026-09）
            let inj_keys = json_counter(e, "injected_keys");
            // 混合分钟不在臂内写快照：统一由 apply_actor_event 之后的
            // recompute_minute_buckets 以重建 SUM 口径重写整分钟
            if (keys > 0 || inj_keys > 0) && !agg_raw_mixed {
                if keys > 0 {
                    conn.execute(
                        UPSERT_MINUTE_SNAPSHOT,
                        params![date, hour, minute, "input_keys", keys, samples, rowid],
                    )?;
                }
                if inj_keys > 0 {
                    conn.execute(
                        UPSERT_MINUTE_SNAPSHOT,
                        params![
                            date,
                            hour,
                            minute,
                            "input_keys_injected",
                            inj_keys,
                            samples,
                            rowid
                        ],
                    )?;
                }
            }
        }
        (EventType::Mouse, EventAction::InputAgg) => {
            let clicks = json_counter(e, "clicks");
            let moves = json_counter(e, "moves");
            let dist = json_counter(e, "move_distance_px");
            let samples = json_counter(e, "samples");
            let inj_clicks = json_counter(e, "injected_clicks");
            // 混合分钟不在臂内写快照：同键盘臂，统一走事后重算
            if (clicks > 0 || inj_clicks > 0 || moves > 0 || dist > 0) && !agg_raw_mixed {
                if clicks > 0 {
                    conn.execute(
                        UPSERT_MINUTE_SNAPSHOT,
                        params![date, hour, minute, "input_clicks", clicks, samples, rowid],
                    )?;
                }
                if inj_clicks > 0 {
                    conn.execute(
                        UPSERT_MINUTE_SNAPSHOT,
                        params![
                            date,
                            hour,
                            minute,
                            "input_clicks_injected",
                            inj_clicks,
                            samples,
                            rowid
                        ],
                    )?;
                }
                if moves > 0 || dist > 0 {
                    conn.execute(
                        UPSERT_MINUTE_SNAPSHOT,
                        params![date, hour, minute, "input_moves", dist, moves, rowid],
                    )?;
                }
            }
        }
        (EventType::Keyboard, EventAction::Press) => {
            conn.execute(
                UPSERT_MINUTE,
                params![date, hour, minute, "input_keys", 1, 1, rowid],
            )?;
            // raw 注入按键（hook 落库 $.injected）同步拆桶
            if json_flag(e, "injected") {
                conn.execute(
                    UPSERT_MINUTE,
                    params![date, hour, minute, "input_keys_injected", 1, 1, rowid],
                )?;
            }
        }
        (EventType::Mouse, EventAction::Click) => {
            conn.execute(
                UPSERT_MINUTE,
                params![date, hour, minute, "input_clicks", 1, 1, rowid],
            )?;
            if json_flag(e, "injected") {
                conn.execute(
                    UPSERT_MINUTE,
                    params![date, hour, minute, "input_clicks_injected", 1, 1, rowid],
                )?;
            }
        }
        (EventType::Window, EventAction::Switch) => {
            conn.execute(
                UPSERT_MINUTE,
                params![date, hour, minute, "window_switches", 1, 1, rowid],
            )?;
            // 审查 MEDIUM：app_name 为 Some("") 的 switch 行不得写 'app:' 桶——
            // 该行会被下次重建/自愈删除。空名一律跳过，与重建口径对齐
            // （统一化后由 match 之后的 app 桶统一块处理，见下）。
        }
        _ => {}
    }

    // ─── actor 证据缓存（0.4.0 F1，0011）：与上方 agg_minute 写入同一事件
    // 同一事务内维护，双路径（增量/重建）逐行同语义；混合分钟在内部
    // 改走重算（口径约束同上） ───
    apply_actor_event(
        conn,
        e,
        &date,
        hour,
        minute,
        rowid,
        agg_minute_bounds
            .as_ref()
            .map(|(a, b)| (a.as_str(), b.as_str())),
    )?;

    // 混合分钟：agg_minute 全部桶行以重建同款 SUM 重算（actor 证据行已在
    // apply_actor_event 内重算；纯形态分钟不走此路径，维持原语义）
    if let Some((ms, me)) = &agg_minute_bounds {
        recompute_minute_buckets(conn, &date, hour, minute, ms, me)?;
    }

    // ─── app 日桶（统一口径，联合审查 P1-1）───
    // 增量路径此前只认 (Window, Switch)，与 rebuild_all / backfill_chunk /
    // events 回退查询（charts.rs top_apps_by_event_types 等）的谓词
    // 「keyboard/mouse/window 且 app_name 非空、无 action 过滤」分叉：
    // TabChange 等带 app_name 的非 switch 行只在重建路径计入，heal/backfill
    // 重写当日后 new_app_surge 基线跳变、同一指标三种读路径两种答案。
    // 统一为同一谓词；input_agg 行的 app_name 是空串（.app("","") 落库），
    // 自然跳过。
    if matches!(
        e.event_type,
        EventType::Keyboard | EventType::Mouse | EventType::Window
    ) && e.app_name.as_deref().is_some_and(|a| !a.is_empty())
    {
        conn.execute(
            UPSERT_DAILY_APP,
            params![date, e.app_name.as_deref().unwrap_or_default(), 1],
        )?;
    }
    Ok(())
}

fn json_counter(e: &Event, key: &str) -> i64 {
    e.event_data
        .as_ref()
        .and_then(|v| v.get(key))
        .and_then(|v| v.as_i64())
        .unwrap_or(0)
}

/// 单个输入事件对 actor 证据缓存（agg_minute_actor）的增量贡献。
///
/// 列语义与 [`apply_event`] 对 agg_minute 对应桶的处理逐条一致：
/// - (Keyboard, InputAgg)：keys_total/keys_injected 快照 MAX 合并
/// - (Mouse, InputAgg)：clicks/scroll 各自快照 MAX 合并
/// - (Keyboard, Press) / (Mouse, Click)：raw 行守卫累加（$.injected 拆注入列）
/// - 其余事件（滚轮 raw 行、switch、系统等）：不写证据缓存（见迁移 0011 头注
///   与 queries::SCROLL_ROW_EXPR 口径说明）
/// - 混合分钟例外（`mixed = Some((mstart, mend))`：同本地分钟已有先到 raw
///   行，界为其 UTC 分钟范围）：快照只覆盖本段、不构成完整分钟，改走
///   [`recompute_minute_actor`] 从 events 以重建同款 SUM 表达式重算本分钟
///   证据行（与全量重建逐行一致）
///
/// 失败向上传播由 [`apply_event`] 的调用方按条跳过并记日志（缓存可重建）。
fn apply_actor_event(
    conn: &Connection,
    e: &Event,
    date: &str,
    hour: i64,
    minute: i64,
    rowid: i64,
    mixed: Option<(&str, &str)>,
) -> rusqlite::Result<()> {
    match (e.event_type, e.event_action) {
        (EventType::Keyboard, EventAction::InputAgg) => {
            let keys = json_counter(e, "keys").max(0);
            let inj = json_counter(e, "injected_keys").max(0);
            if keys > 0 || inj > 0 {
                if let Some((ms, me)) = mixed {
                    recompute_minute_actor(conn, date, hour, minute, ms, me)?;
                } else {
                    conn.execute(
                        UPSERT_ACTOR_SNAPSHOT,
                        params![date, hour, minute, keys, inj, 0, 0, 0, 0, rowid],
                    )?;
                }
            }
        }
        (EventType::Mouse, EventAction::InputAgg) => {
            let clicks = json_counter(e, "clicks").max(0);
            let inj_clicks = json_counter(e, "injected_clicks").max(0);
            let scroll = json_counter(e, "scroll_ticks").max(0);
            let inj_scroll = json_counter(e, "injected_scroll_ticks").max(0);
            if clicks > 0 || inj_clicks > 0 || scroll > 0 || inj_scroll > 0 {
                if let Some((ms, me)) = mixed {
                    recompute_minute_actor(conn, date, hour, minute, ms, me)?;
                } else {
                    conn.execute(
                        UPSERT_ACTOR_SNAPSHOT,
                        params![
                            date, hour, minute, 0, 0, clicks, inj_clicks, scroll, inj_scroll, rowid
                        ],
                    )?;
                }
            }
        }
        (EventType::Keyboard, EventAction::Press) => {
            let inj = i64::from(json_flag(e, "injected"));
            conn.execute(
                UPSERT_ACTOR_ADD,
                params![date, hour, minute, 1, inj, 0, 0, 0, 0, rowid],
            )?;
        }
        (EventType::Mouse, EventAction::Click) => {
            let inj = i64::from(json_flag(e, "injected"));
            conn.execute(
                UPSERT_ACTOR_ADD,
                params![date, hour, minute, 0, 0, 1, inj, 0, 0, rowid],
            )?;
        }
        _ => {}
    }
    Ok(())
}

/// 测试专用入口：按事件时间戳自行解析本地分钟桶（含混合分钟探测）后调用
/// [`apply_actor_event`]（与 apply_event 的桶计算同构）。生产路径不走这里——
/// 增量写入统一在 [`apply_event`] 内部与 events 落库同事务完成。
#[cfg(test)]
pub(crate) fn apply_actor_event_for_test(
    conn: &Connection,
    e: &Event,
    rowid: i64,
) -> rusqlite::Result<()> {
    let ts = match chrono::DateTime::parse_from_rfc3339(&e.timestamp) {
        Ok(t) => t,
        Err(_) => return Ok(()),
    };
    let local = ts.with_timezone(&chrono::Local);
    let date = local.format("%Y-%m-%d").to_string();
    let hour = local.hour() as i64;
    let minute = local.minute() as i64;
    let mixed = detect_mixed_minute(conn, e, &ts, rowid);
    apply_actor_event(
        conn,
        e,
        &date,
        hour,
        minute,
        rowid,
        mixed.as_ref().map(|(a, b)| (a.as_str(), b.as_str())),
    )
}

fn json_flag(e: &Event, key: &str) -> bool {
    e.event_data
        .as_ref()
        .and_then(|v| v.get(key))
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

/// 事件时刻所在 UTC 分钟的 [start, end) RFC3339 界。本地分钟桶与 UTC 分钟
/// 界一一对应（真实时区偏移均为整分钟），界作 timestamp 字符串范围谓词供
/// idx_events_timestamp 走索引（钟面函数谓词会全表扫描）。
fn utc_minute_bounds(ts: &chrono::DateTime<chrono::FixedOffset>) -> (String, String) {
    let start = ts.with_timezone(&Utc)
        - chrono::Duration::seconds(i64::from(ts.second()))
        - chrono::Duration::nanoseconds(i64::from(ts.nanosecond()));
    let end = start + chrono::Duration::minutes(1);
    (start.to_rfc3339(), end.to_rfc3339())
}

/// 混合分钟探测（增量入口共用）：同本地分钟内若已有先于本行的 raw
/// press/click 行（tray 按 input_counts_only 中途切换重启采集器即产生），
/// 本行快照只覆盖切换后本段、不构成完整分钟：MAX 覆盖会与全量重建
/// （同桶 SUM 两形态）及今日按键读 events SUM 的消费面分叉。
/// 返回 Some((mstart, mend)) = 命中混合（含该分钟 UTC 界供重算谓词）；
/// None = 纯形态分钟（维持快照语义）。生产 writer 先落库后 apply_event
/// （同事务成对，见 insert_events_with_agg_tx），本行 rowid 已分配；
/// rowid=0（未知）探不出先到行。非 input_agg 输入行不探测。
fn detect_mixed_minute(
    conn: &Connection,
    e: &Event,
    ts: &chrono::DateTime<chrono::FixedOffset>,
    rowid: i64,
) -> Option<(String, String)> {
    if rowid <= 0
        || !matches!(e.event_action, EventAction::InputAgg)
        || !matches!(e.event_type, EventType::Keyboard | EventType::Mouse)
    {
        return None;
    }
    let (ms, me) = utc_minute_bounds(ts);
    if minute_has_raw_before(conn, &ms, &me, e.event_type.as_str(), rowid) {
        Some((ms, me))
    } else {
        None
    }
}

/// 重算类写路径的自持事务守卫：仅 autocommit 时 BEGIN IMMEDIATE 并在
/// 结束时提交/回滚；调用方已有事务时不嵌套（原子性由外层负责）。
fn with_self_tx<T>(
    conn: &Connection,
    f: impl FnOnce() -> rusqlite::Result<T>,
) -> rusqlite::Result<T> {
    let own_tx = conn.is_autocommit();
    if own_tx {
        conn.execute_batch("BEGIN IMMEDIATE")?;
    }
    let r = f();
    if own_tx {
        match r {
            Ok(_) => conn.execute_batch("COMMIT")?,
            Err(_) => {
                let _ = conn.execute_batch("ROLLBACK");
            }
        }
    }
    r
}

/// 混合分钟探测：该本地分钟内是否存在先于 rowid 落库的 raw press/click 行。
/// 只查 press/click——raw move/scroll/switch 行不参与 input_keys/input_clicks
/// 桶的 SUM（moves 桶仅有 input_agg 贡献），不构成混合。
fn minute_has_raw_before(
    conn: &Connection,
    mstart: &str,
    mend: &str,
    event_type: &str,
    rowid: i64,
) -> bool {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM events WHERE event_type = ?1 \
         AND event_action IN ('press','click') \
         AND timestamp >= ?2 AND timestamp < ?3 AND id < ?4)",
        params![event_type, mstart, mend, rowid],
        |r| r.get::<_, i64>(0),
    )
    .unwrap_or(0)
        > 0
}

/// 混合分钟重算（agg_minute）：以与 rebuild_all 逐字相同的 SUM 表达式从
/// events 重写该本地分钟的全部桶行——统一口径为事件 SUM（raw 每行计数 +
/// input_agg 快照同桶求和），与今日按键读 events SUM 的消费面一致；
/// 纯形态分钟不走此路径（维持 MAX 覆盖/守卫累加的既有幂等语义）。
/// 事务守卫见 with_self_tx（writer 同事务调用时不重复 BEGIN）。
fn recompute_minute_buckets(
    conn: &Connection,
    date: &str,
    hour: i64,
    minute: i64,
    mstart: &str,
    mend: &str,
) -> rusqlite::Result<()> {
    with_self_tx(conn, || {
        conn.execute(
            "DELETE FROM agg_minute WHERE date = ?1 AND hour = ?2 AND minute = ?3",
            params![date, hour, minute],
        )?;
        conn.execute_batch(&format!(
            "INSERT INTO agg_minute (date, hour, minute, bucket_id, sum_value, count_value, max_event_rowid)
             WITH m AS (
               SELECT MAX(id) AS maxid,
                      SUM({keys_row}) AS keys,
                      SUM({clicks_row}) AS clicks,
                      SUM({inj_keys_row}) AS ikeys,
                      SUM({inj_clicks_row}) AS iclicks,
                      {cnt_dist} AS dist,
                      {cnt_samples_keys} AS ckeys,
                      {cnt_samples_clicks} AS cclicks,
                      {cnt_samples_inj_keys} AS cinkeys,
                      {cnt_samples_inj_clicks} AS cinclks,
                      {cnt_moves} AS cmoves,
                      {cnt_switches} AS switches
               FROM events
               WHERE event_type IN ('keyboard','mouse','window')
                 AND timestamp >= '{mstart}' AND timestamp < '{mend}'
             ),
             b AS (
               SELECT 'input_keys' AS bk, keys AS s, ckeys AS c, maxid FROM m WHERE keys > 0
               UNION ALL
               SELECT 'input_clicks', clicks, cclicks, maxid FROM m WHERE clicks > 0
               UNION ALL
               SELECT 'input_moves', dist, cmoves, maxid FROM m WHERE dist > 0 OR cmoves > 0
               UNION ALL
               SELECT 'window_switches', switches, switches, maxid FROM m WHERE switches > 0
               UNION ALL
               SELECT 'input_keys_injected', ikeys, cinkeys, maxid FROM m WHERE ikeys > 0
               UNION ALL
               SELECT 'input_clicks_injected', iclicks, cinclks, maxid FROM m WHERE iclicks > 0
             )
             SELECT '{date}', {hour}, {minute}, bk, s, c, maxid FROM b;",
            keys_row = crate::queries::KEYS_ROW_EXPR,
            clicks_row = crate::queries::CLICKS_ROW_EXPR,
            inj_keys_row = crate::queries::INJECTED_KEYS_ROW_EXPR,
            inj_clicks_row = crate::queries::INJECTED_CLICKS_ROW_EXPR,
            cnt_dist = CNT_DIST_EXPR,
            cnt_samples_keys = CNT_SAMPLES_KEYS_EXPR,
            cnt_samples_clicks = CNT_SAMPLES_CLICKS_EXPR,
            cnt_samples_inj_keys = CNT_SAMPLES_INJ_KEYS_EXPR,
            cnt_samples_inj_clicks = CNT_SAMPLES_INJ_CLICKS_EXPR,
            cnt_moves = CNT_MOVES_EXPR,
            cnt_switches = CNT_SWITCHES_EXPR,
            mstart = mstart,
            mend = mend,
            date = date,
            hour = hour,
            minute = minute,
        ))?;
        Ok(())
    })
}

/// 混合分钟重算（agg_minute_actor）：以与 rebuild_actor_cache 逐字相同的
/// SUM 表达式从 events 重写该本地分钟的证据行（事务守卫见 with_self_tx）。
fn recompute_minute_actor(
    conn: &Connection,
    date: &str,
    hour: i64,
    minute: i64,
    mstart: &str,
    mend: &str,
) -> rusqlite::Result<()> {
    with_self_tx(conn, || {
        conn.execute(
            "DELETE FROM agg_minute_actor WHERE date = ?1 AND hour = ?2 AND minute = ?3",
            params![date, hour, minute],
        )?;
        conn.execute_batch(&format!(
            "INSERT INTO agg_minute_actor (date, hour, minute, keys_total, keys_injected,
                                          clicks_total, clicks_injected, scroll_total, scroll_injected,
                                          max_event_rowid)
             WITH m AS (
               SELECT MAX(id) AS maxid,
                      SUM({keys_row}) AS keys,
                      SUM({clicks_row}) AS clicks,
                      SUM({inj_keys_row}) AS ikeys,
                      SUM({inj_clicks_row}) AS iclicks,
                      SUM({scroll_row}) AS scroll,
                      SUM({inj_scroll_row}) AS iscroll
               FROM events
               WHERE event_type IN ('keyboard','mouse')
                 AND timestamp >= '{mstart}' AND timestamp < '{mend}'
             )
             SELECT '{date}', {hour}, {minute}, keys, ikeys, clicks, iclicks, scroll, iscroll, maxid FROM m
             WHERE keys > 0 OR ikeys > 0 OR clicks > 0 OR iclicks > 0 OR scroll > 0 OR iscroll > 0;",
            keys_row = crate::queries::KEYS_ROW_EXPR,
            clicks_row = crate::queries::CLICKS_ROW_EXPR,
            inj_keys_row = crate::queries::INJECTED_KEYS_ROW_EXPR,
            inj_clicks_row = crate::queries::INJECTED_CLICKS_ROW_EXPR,
            scroll_row = crate::queries::SCROLL_ROW_EXPR,
            inj_scroll_row = crate::queries::INJECTED_SCROLL_ROW_EXPR,
            mstart = mstart,
            mend = mend,
            date = date,
            hour = hour,
            minute = minute,
        ))?;
        Ok(())
    })
}

impl super::Database {
    /// 增量维护聚合缓存：对一批刚落库的事件做 agg_minute / agg_daily UPSERT。
    ///
    /// `rowids` 与 `events` 一一对应（由 [`super::Database::insert_events`]
    /// 返回的 events 表 rowid；写入失败的行为 0）。rowid 用于 agg_minute 行的
    /// max_event_rowid 幂等防护：同事件重复投递（如分块回填重算后迟到的增量）
    /// 不会重复累计。
    ///
    /// 由 writer 线程在每次 flush 后调用。失败仅 log（缓存可由 backfill 重建，
    /// 不影响原始写入）。
    ///
    /// perf3（2026-09-09）：整批包进**一个事务**——旧实现每条 UPSERT 独立隐式
    /// 提交，实测 ~383µs/事件被提交开销主导（perf3-longrun P1：300 事件/批
    /// ~115ms）；单事务后每批一次提交，突发写入吞吐提升一个数量级。
    pub fn update_agg(&self, events: &[Event], rowids: &[i64]) {
        self.with_writer(
            |conn| match conn.unchecked_transaction() {
                Ok(tx) => {
                    for (i, e) in events.iter().enumerate() {
                        let rowid = rowids.get(i).copied().unwrap_or(0);
                        if let Err(err) = apply_event(&tx, e, rowid) {
                            log::warn!("agg 缓存增量更新失败（跳过单条）: {err}");
                        }
                    }
                    if let Err(err) = tx.commit() {
                        log::warn!("agg 缓存增量更新提交失败（本批跳过，可由回填补齐）: {err}");
                    }
                }
                Err(err) => log::warn!("agg 缓存增量更新事务创建失败（本批跳过）: {err}"),
            },
            || log::warn!("agg 缓存增量更新跳过：写连接不可用"),
        );
    }

    /// 全量重建聚合读缓存（写连接上执行）。返回 agg_minute 行数，失败返回 0。
    /// 派生数据操作：events 原样不动。
    pub fn rebuild_agg(&self) -> usize {
        self.with_writer(
            |conn| {
                crate::db::agg::rebuild_all(conn).unwrap_or_else(|e| {
                    log::warn!("agg 缓存全量重建失败: {e}");
                    0
                })
            },
            || 0,
        )
    }

    /// actor 证据缓存定向重建（0.4.0 F1）。只重算 agg_minute_actor，
    /// events 与其余聚合缓存原样不动。返回缓存行数，失败返回 0。
    pub fn rebuild_actor_agg(&self) -> usize {
        self.with_writer(
            |conn| {
                crate::db::agg::rebuild_actor_cache(conn).unwrap_or_else(|e| {
                    log::warn!("actor 证据缓存重建失败: {e}");
                    0
                })
            },
            || 0,
        )
    }

    /// 欠聚合核对 + 自愈（写连接上执行）。返回自愈的日期数（0 = 一致，无需自愈）。
    /// 供每日 maintenance 与需要显式触发自愈的调用方使用；启动路径在
    /// [`super::Database::open`] 的后台线程里做（不阻塞启动）。
    pub fn heal_agg_lag(&self) -> usize {
        self.with_writer(
            |conn| {
                crate::db::agg::heal_under_agg(conn).unwrap_or_else(|e| {
                    log::warn!("欠聚合自愈失败（下次维护重试）: {e}");
                    0
                })
            },
            || 0,
        )
    }
}

/// 从 events 全量重建聚合缓存（幂等）。返回重建的 agg_minute 行数。
///
/// 兼容两种事件形态：raw（press/click/switch 逐行）与 input_agg（计数 JSON）。
pub fn rebuild_all(conn: &Connection) -> crate::Result<usize> {
    let off = crate::queries::LOCAL_MODIFIER_AT_EVENT;
    // 联合审查 GPT#9a：整体原子化——先删后插的中间态（已删未建/半建）此前
    // 对并发读者可见，失败后部分查询走缓存部分回退 events，答案互相矛盾。
    // 自持事务：仅当连接处于 autocommit（调用方未包事务）时开启；外层已有
    // 事务则维持原状（原子性由外层负责）。失败时整个回滚到重建前缓存。
    let own_tx = conn.is_autocommit();
    if own_tx {
        conn.execute_batch("BEGIN IMMEDIATE")?;
    }
    let result = rebuild_all_inner(conn, off);
    if own_tx {
        match result {
            Ok(_) => conn.execute_batch("COMMIT")?,
            Err(_) => {
                let _ = conn.execute_batch("ROLLBACK");
            }
        }
    }
    result
}

fn rebuild_all_inner(conn: &Connection, off: &str) -> crate::Result<usize> {
    conn.execute("DELETE FROM agg_minute", [])?;
    conn.execute("DELETE FROM agg_daily WHERE bucket_id LIKE 'app:%'", [])?;
    // actor 证据缓存随重建一并重算（同一事务，见外层 rebuild_all 的原子化）；
    // 列语义与 apply_actor_event 增量路径逐条一致（同一组 *_ROW_EXPR 宏）。
    conn.execute("DELETE FROM agg_minute_actor", [])?;

    // 分钟桶：一次 GROUP BY 本地分钟扫描，产出 4 类 bucket 行（单语句 UNION ALL，
    // 共享同一 CTE 扫描）。
    // input_agg 行的计数从 event_data JSON 提取；raw 行按行计数。
    conn.execute_batch(&format!(
        "INSERT INTO agg_minute (date, hour, minute, bucket_id, sum_value, count_value, max_event_rowid)
         WITH m AS (
           SELECT substr(datetime(timestamp, '{off}'), 1, 10) AS d,
                  CAST(substr(datetime(timestamp, '{off}'), 12, 2) AS INTEGER) AS h,
                  CAST(substr(datetime(timestamp, '{off}'), 15, 2) AS INTEGER) AS mi,
                  MAX(id) AS maxid,
                  SUM({keys_row}) AS keys,
                  SUM({clicks_row}) AS clicks,
                  SUM({inj_keys_row}) AS ikeys,
                  SUM({inj_clicks_row}) AS iclicks,
                  {cnt_dist} AS dist,
                  {cnt_samples_keys} AS ckeys,
                  {cnt_samples_clicks} AS cclicks,
                  {cnt_samples_inj_keys} AS cinkeys,
                  {cnt_samples_inj_clicks} AS cinclks,
                  {cnt_moves} AS cmoves,
                  {cnt_switches} AS switches
           FROM events
           WHERE event_type IN ('keyboard','mouse','window')
           GROUP BY d, h, mi
         ),
         b AS (
           SELECT d, h, mi, 'input_keys' AS bk, keys AS s, ckeys AS c, maxid FROM m WHERE keys > 0
           UNION ALL
           SELECT d, h, mi, 'input_clicks', clicks, cclicks, maxid FROM m WHERE clicks > 0
           UNION ALL
           SELECT d, h, mi, 'input_moves', dist, cmoves, maxid FROM m WHERE dist > 0 OR cmoves > 0
           UNION ALL
           SELECT d, h, mi, 'window_switches', switches, switches, maxid FROM m WHERE switches > 0
           UNION ALL
           SELECT d, h, mi, 'input_keys_injected', ikeys, cinkeys, maxid FROM m WHERE ikeys > 0
           UNION ALL
           SELECT d, h, mi, 'input_clicks_injected', iclicks, cinclks, maxid FROM m WHERE iclicks > 0
         )
         SELECT d, h, mi, bk, s, c, maxid FROM b;

         INSERT INTO agg_daily (date, bucket_id, sum_value, count_value)
         SELECT substr(datetime(timestamp, '{off}'), 1, 10),
                'app:' || app_name,
                NULL,
                COUNT(*)
         FROM events
         WHERE app_name IS NOT NULL AND app_name != '' AND event_type IN ('keyboard','mouse','window')
         GROUP BY 1, 2;",
        off = off,
        keys_row = crate::queries::KEYS_ROW_EXPR,
        clicks_row = crate::queries::CLICKS_ROW_EXPR,
        inj_keys_row = crate::queries::INJECTED_KEYS_ROW_EXPR,
        inj_clicks_row = crate::queries::INJECTED_CLICKS_ROW_EXPR,
        cnt_dist = CNT_DIST_EXPR,
        cnt_samples_keys = CNT_SAMPLES_KEYS_EXPR,
        cnt_samples_clicks = CNT_SAMPLES_CLICKS_EXPR,
        cnt_samples_inj_keys = CNT_SAMPLES_INJ_KEYS_EXPR,
        cnt_samples_inj_clicks = CNT_SAMPLES_INJ_CLICKS_EXPR,
        cnt_moves = CNT_MOVES_EXPR,
        cnt_switches = CNT_SWITCHES_EXPR,
    ))?;

    // actor 证据缓存重建：与 agg_minute 共享同一组表达式宏（keys/clicks/
    // injected 列逐字一致；scroll 列见 SCROLL_ROW_EXPR——仅 mouse input_agg
    // 行，与 presence 权威口径一致）。有任一证据的分钟才落行。
    conn.execute_batch(&format!(
        "INSERT INTO agg_minute_actor (date, hour, minute, keys_total, keys_injected,
                                      clicks_total, clicks_injected, scroll_total, scroll_injected,
                                      max_event_rowid)
         WITH m AS (
           SELECT substr(datetime(timestamp, '{off}'), 1, 10) AS d,
                  CAST(substr(datetime(timestamp, '{off}'), 12, 2) AS INTEGER) AS h,
                  CAST(substr(datetime(timestamp, '{off}'), 15, 2) AS INTEGER) AS mi,
                  MAX(id) AS maxid,
                  SUM({keys_row}) AS keys,
                  SUM({clicks_row}) AS clicks,
                  SUM({inj_keys_row}) AS ikeys,
                  SUM({inj_clicks_row}) AS iclicks,
                  SUM({scroll_row}) AS scroll,
                  SUM({inj_scroll_row}) AS iscroll
           FROM events
           WHERE event_type IN ('keyboard','mouse')
           GROUP BY d, h, mi
         )
         SELECT d, h, mi, keys, ikeys, clicks, iclicks, scroll, iscroll, maxid FROM m
         WHERE keys > 0 OR ikeys > 0 OR clicks > 0 OR iclicks > 0 OR scroll > 0 OR iscroll > 0;",
        off = off,
        keys_row = crate::queries::KEYS_ROW_EXPR,
        clicks_row = crate::queries::CLICKS_ROW_EXPR,
        inj_keys_row = crate::queries::INJECTED_KEYS_ROW_EXPR,
        inj_clicks_row = crate::queries::INJECTED_CLICKS_ROW_EXPR,
        scroll_row = crate::queries::SCROLL_ROW_EXPR,
        inj_scroll_row = crate::queries::INJECTED_SCROLL_ROW_EXPR,
    ))?;

    let n = conn.query_row("SELECT COUNT(*) FROM agg_minute", [], |r| r.get(0))?;
    Ok(n)
}

/// 欠聚合廉价核对（P1 自愈门槛，两条聚合查询）：返回本地日期列表——该日
/// events 中"应有聚合贡献"的原始行（press/click/switch/input_agg）最大 id
/// 大于该日 agg_minute 已记录的 MAX(max_event_rowid)（含该日完全没有聚合行
/// 的情况）。
///
/// 审查 33-F5：核对口径加入 input_agg 行。默认 Minute 粒度下输入只以
/// event_action='input_agg' 落库，旧口径（仅 press/click/switch）对
/// input_agg-only 日永远检不出欠聚合——不回填也不重算；若该日还残留部分
/// 旧聚合行，has_minute_for_date=true 会让图表走缓存读路径直接漏算。
/// 收敛性：重算路径写入桶内 MAX(id)（含 input_agg 行）；input_agg 行是
/// UPSERT（同分钟同类型不改 id），只有新分钟才产生更大 id——新分钟会再次
/// 触发当日核对并由下次自愈收敛，不会产生历史日的持续误报。
///
/// **启动延迟（交叉审查 P3）**：本函数在 `Database::open` 主线程同步调用。
/// 无下界时是 events 全表 GROUP BY 扫描（362 万行库上秒级启动回归）。这里加
/// `timestamp >= cutoff` 的**日期下界**（自愈窗口 7 天）：谓词可走
/// idx_events_timestamp 索引区间，只核对最近 7 天，启动核对降到毫秒级。
/// **取舍（文档化）**：7 天前的欠聚合日期不再被启动/维护自愈——这类日期只能
/// 来自事务化之前的历史遗留 kill，且日常使用中不会再看；需要修复历史数据时
/// 显式调用 `rebuild_agg`（或删掉 agg_minute 触发懒回填）即可全量重建。
pub fn under_agg_dates(conn: &Connection) -> Vec<String> {
    let off = crate::queries::LOCAL_MODIFIER_AT_EVENT;
    // 只核对最近 7 天（见上方取舍说明）；cutoff 用 UTC RFC3339 与 timestamp 列同构比较。
    let cutoff = (Utc::now() - chrono::Duration::days(7)).to_rfc3339();
    // agg 侧改为**按候选日**取 MAX(max_event_rowid)：旧实现
    // `SELECT date, MAX(...) FROM agg_minute GROUP BY date` 是 agg_minute 全表
    // 扫描，成本随库龄线性增长（24 万行实测 1.3s，每次开库 + 每日维护各一次）。
    // 现按 (date,hour,minute,bucket_id) 主键索引对窗口内候选日（至多 8 个）
    // 做索引区间查询，成本只随 7 天窗口的聚合行数走、与库龄无关。
    let sql = format!(
        "SELECT e.d FROM (
           SELECT substr(datetime(timestamp, '{off}'), 1, 10) AS d, MAX(id) AS maxid
           FROM events
           WHERE event_type IN ('keyboard','mouse','window')
             AND event_action IN ('press','click','switch','input_agg')
             AND timestamp >= '{cutoff}'
           GROUP BY d
         ) e
         WHERE COALESCE((SELECT MAX(COALESCE(max_event_rowid, 0))
                         FROM agg_minute WHERE date = e.d), 0) < e.maxid
         ORDER BY e.d",
        off = off,
        cutoff = cutoff,
    );
    let mut stmt = match conn.prepare(&sql) {
        Ok(s) => s,
        Err(e) => {
            log::warn!("欠聚合核对查询失败（跳过本次自愈检查）: {e}");
            return Vec::new();
        }
    };
    let rows = stmt.query_map([], |r| r.get::<_, String>(0));
    match rows {
        Ok(it) => it.filter_map(|r| r.ok()).collect(),
        Err(e) => {
            log::warn!("欠聚合核对读取失败（跳过本次自愈检查）: {e}");
            Vec::new()
        }
    }
}

/// 欠聚合自愈：对 [`under_agg_dates`] 列出的每个本地日期逐小时重算
/// [`backfill_chunk`]（幂等：DELETE 该块聚合行 + 从 events 重算，小时块无事件
/// 时廉价跳过）。返回自愈的日期数。
pub fn heal_under_agg(conn: &Connection) -> crate::Result<usize> {
    let dates = under_agg_dates(conn);
    for date in &dates {
        for h in 0..24i64 {
            backfill_chunk(conn, date, h)?;
        }
        // daily_agg 同步补算（缺陷修复）：7 天自愈窗口此前只修 agg_minute，
        // 跨 ≥2 个午夜唤醒后迟到的旧日事件会让 daily_agg 永久读 0，与
        // agg_minute 分叉。对齐 recompute_recent_days 的 2 天窗口与这里的
        // 7 天自愈窗口，命中日期一并重算，两套派生缓存保持一致。
        if let Err(e) = crate::daily_agg::recompute_day(conn, date) {
            log::warn!("欠聚合日期 {date} 的 daily_agg 补算失败: {e}");
        }
        log::info!("欠聚合日期 {date} 已重算自愈");
    }
    Ok(dates.len())
}

/// 懒回填：agg 全空而 events 非空时执行一次全量重建（存量库首开路径）。
/// 返回是否执行了回填。
///
/// perf3（2026-09-09）：改为**分块（本地 date,hour）回填**——旧实现单事务全量
/// 重建，1M 事件存量库首开会把 `Database::open` 阻塞 10.5 分钟（perf3-open 实测
/// 631,594 ms，P0）。分块后每个 (date,hour) 块在一个短事务内原子完成
/// （DELETE 该块聚合行 + 从 events 重算 INSERT），writer 的增量 update_agg 在
/// 块间穿插执行不被饿死；进度记录在 metadata.agg_backfill_cursor，中断后下次
/// 打开自动续跑。
pub fn backfill_if_needed(conn: &Connection) -> bool {
    if !backfill_needed(conn) {
        return false;
    }
    match backfill_all(conn) {
        Ok(n) => {
            log::info!("agg 缓存懒回填完成（{} 行 agg_minute）", n);
            true
        }
        Err(e) => {
            log::warn!("agg 缓存懒回填失败（get_anomalies 将回退 events 现算）: {e}");
            false
        }
    }
}

/// actor 证据缓存定向重建（不动 agg_minute / agg_daily，幂等）：
/// DELETE 全表后按 rebuild 同一 SQL 重算。供存量库首开 bootstrap
/// （migration 0011 后缓存为空）与 `recompute-actors` CLI 使用——
/// 只写本派生缓存，events 原样不动（GPT 验收门禁：历史回算不改 events）。
pub fn rebuild_actor_cache(conn: &Connection) -> crate::Result<usize> {
    rebuild_actor_cache_inner(conn, None)
}

/// [`rebuild_actor_cache`] 的本地日期范围版（含两端）：只重算
/// `[from, to]` 内日期的缓存行。`kynoptic-ctl recompute-actors --from --to` 用。
pub fn rebuild_actor_cache_range(conn: &Connection, from: &str, to: &str) -> crate::Result<usize> {
    rebuild_actor_cache_inner(conn, Some((from, to)))
}

fn rebuild_actor_cache_inner(
    conn: &Connection,
    date_range: Option<(&str, &str)>,
) -> crate::Result<usize> {
    let off = crate::queries::LOCAL_MODIFIER_AT_EVENT;
    let own_tx = conn.is_autocommit();
    if own_tx {
        conn.execute_batch("BEGIN IMMEDIATE")?;
    }
    let r = (|| -> crate::Result<usize> {
        match date_range {
            Some((from, to)) => {
                conn.execute(
                    "DELETE FROM agg_minute_actor WHERE date >= ?1 AND date <= ?2",
                    params![from, to],
                )?;
            }
            None => {
                conn.execute("DELETE FROM agg_minute_actor", [])?;
            }
        }
        let range_where = match date_range {
            // 日期列由本地钟面生成，字符串比较即字典序（rebuild 同款）
            Some((from, to)) => format!(
                " WHERE substr(datetime(timestamp, '{off}'), 1, 10) BETWEEN '{from}' AND '{to}'"
            ),
            None => String::new(),
        };
        conn.execute_batch(&format!(
            "INSERT INTO agg_minute_actor (date, hour, minute, keys_total, keys_injected,
                                          clicks_total, clicks_injected, scroll_total, scroll_injected,
                                           max_event_rowid)
             WITH m AS (
               SELECT substr(datetime(timestamp, '{off}'), 1, 10) AS d,
                      CAST(substr(datetime(timestamp, '{off}'), 12, 2) AS INTEGER) AS h,
                      CAST(substr(datetime(timestamp, '{off}'), 15, 2) AS INTEGER) AS mi,
                      MAX(id) AS maxid,
                      SUM({keys_row}) AS keys,
                      SUM({clicks_row}) AS clicks,
                      SUM({inj_keys_row}) AS ikeys,
                      SUM({inj_clicks_row}) AS iclicks,
                      SUM({scroll_row}) AS scroll,
                      SUM({inj_scroll_row}) AS iscroll
               FROM events
               WHERE event_type IN ('keyboard','mouse'){range_where}
               GROUP BY d, h, mi
             )
             SELECT d, h, mi, keys, ikeys, clicks, iclicks, scroll, iscroll, maxid FROM m
             WHERE keys > 0 OR ikeys > 0 OR clicks > 0 OR iclicks > 0 OR scroll > 0 OR iscroll > 0;",
            off = off,
            range_where = range_where,
            keys_row = crate::queries::KEYS_ROW_EXPR,
            clicks_row = crate::queries::CLICKS_ROW_EXPR,
            inj_keys_row = crate::queries::INJECTED_KEYS_ROW_EXPR,
            inj_clicks_row = crate::queries::INJECTED_CLICKS_ROW_EXPR,
            scroll_row = crate::queries::SCROLL_ROW_EXPR,
            inj_scroll_row = crate::queries::INJECTED_SCROLL_ROW_EXPR,
        ))?;
        Ok(conn.query_row("SELECT COUNT(*) FROM agg_minute_actor", [], |r| r.get(0))?)
    })();
    if own_tx {
        match r {
            Ok(_) => conn.execute_batch("COMMIT")?,
            Err(_) => {
                let _ = conn.execute_batch("ROLLBACK");
            }
        }
    }
    r
}

/// actor 证据缓存 bootstrap 门槛：缓存空而 events 有输入行 → 存量库首开需补齐。
/// 查询层对缺行日期回退 events 现算（queries/actor.rs），此 bootstrap 只是提速。
pub fn actor_backfill_needed(conn: &Connection) -> bool {
    let has_events: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM events WHERE event_type IN ('keyboard','mouse'))",
            [],
            |r| r.get(0),
        )
        .unwrap_or(false);
    let has_actor: bool = conn
        .query_row("SELECT EXISTS(SELECT 1 FROM agg_minute_actor)", [], |r| {
            r.get(0)
        })
        .unwrap_or(false);
    has_events && !has_actor
}

/// 回填门槛检查（便宜：两条 EXISTS）。agg 非空时不回填——既有增量维护负责
/// 新鲜度；若上次分块回填中断，靠 metadata.agg_backfill_cursor 续跑判定。
pub fn backfill_needed(conn: &Connection) -> bool {
    let has_events: bool = conn
        .query_row("SELECT EXISTS(SELECT 1 FROM events)", [], |r| r.get(0))
        .unwrap_or(false);
    let has_agg: bool = conn
        .query_row("SELECT EXISTS(SELECT 1 FROM agg_minute)", [], |r| r.get(0))
        .unwrap_or(false);
    has_events && !has_agg
}

const BACKFILL_CURSOR_DONE: &str = "done";

/// 上次分块回填是否中断（cursor 存在且未到 done）。供 Database::open 决定续跑。
pub fn read_cursor_incomplete(conn: &Connection) -> bool {
    read_cursor(conn).is_some()
}

fn read_cursor(conn: &Connection) -> Option<String> {
    conn.query_row(
        "SELECT value FROM metadata WHERE key = 'agg_backfill_cursor'",
        [],
        |r| r.get(0),
    )
    .ok()
    .filter(|v| v != BACKFILL_CURSOR_DONE)
}

fn write_cursor(conn: &Connection, value: &str) {
    let _ = conn.execute(
        "INSERT INTO metadata (key, value) VALUES ('agg_backfill_cursor', ?1)
         ON CONFLICT(key) DO UPDATE SET value = ?1",
        params![value],
    );
}

/// 本地 (date, hour) 块 → UTC RFC3339 `[start, end)` 边界（sargable，走
/// idx_events_timestamp）。
///
/// DST 语义（审查修复 2026-09）：
/// - **start 钟面缺失**（春季跳变的缺口小时，如本地 02:00 不存在）：返回
///   None，整块跳过——缺口小时里不存在任何钟面时刻，不可能有事件归属该
///   本地小时，跳过即正确（不是静默丢数据）。
/// - **end 钟面缺失**：回退为 `start + 1 小时` 的真实下一时刻——旧回退把
///   naive 钟面直接当 UTC 解释，窗口多出一小时并与下一小时桶完全重叠，
///   重算时相邻桶重复计入同一批事件。
fn hour_bounds(date: &str, hour: i64) -> Option<(String, String)> {
    use chrono::{NaiveDate, TimeZone};
    let day = NaiveDate::parse_from_str(date, "%Y-%m-%d").ok()?;
    let naive = day.and_hms_opt(hour as u32, 0, 0)?;
    let start = Local
        .from_local_datetime(&naive)
        .earliest()?
        .with_timezone(&Utc);
    let end_naive = if hour >= 23 {
        day.succ_opt()?.and_hms_opt(0, 0, 0)?
    } else {
        day.and_hms_opt(hour as u32 + 1, 0, 0)?
    };
    let end = Local
        .from_local_datetime(&end_naive)
        .earliest()
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or(start + chrono::Duration::hours(1));
    Some((start.to_rfc3339(), end.to_rfc3339()))
}

fn local_day_bounds(date: &str) -> Option<(String, String)> {
    crate::queries::local_day_range(date)
}

/// 重算单个本地 (date, hour) 块的 agg_minute 行 + （若是该日最后一小时）
/// 该日 agg_daily app 行。单事务原子：并发 writer 的增量 upsert 要么整体在
/// 事务前提交（被 DELETE 抹掉后由重算覆盖——重算扫描包含该事件），要么在
/// 事务后提交（该事件 rowid <= 桶的 max_event_rowid，被 UPSERT_MINUTE 的
/// WHERE 守卫跳过，不重复累计）。两种交错均正确。
fn backfill_chunk(conn: &Connection, date: &str, hour: i64) -> crate::Result<()> {
    let Some((start, end)) = hour_bounds(date, hour) else {
        return Ok(());
    };
    let (dstart, dend) = local_day_bounds(date).unwrap_or((start.clone(), end.clone()));
    let off = crate::queries::LOCAL_MODIFIER_AT_EVENT;
    conn.execute_batch("BEGIN IMMEDIATE;")?;
    let r = (|| -> crate::Result<()> {
        conn.execute(
            "DELETE FROM agg_minute WHERE date = ?1 AND hour = ?2",
            params![date, hour],
        )?;
        conn.execute_batch(&format!(
            "INSERT INTO agg_minute (date, hour, minute, bucket_id, sum_value, count_value, max_event_rowid)
             WITH m AS (
               SELECT CAST(substr(datetime(timestamp, '{off}'), 15, 2) AS INTEGER) AS mi,
                      MAX(id) AS maxid,
                      SUM({keys_row}) AS keys,
                      SUM({clicks_row}) AS clicks,
                      SUM({inj_keys_row}) AS ikeys,
                      SUM({inj_clicks_row}) AS iclicks,
                      {cnt_dist} AS dist,
                      {cnt_samples_keys} AS ckeys,
                      {cnt_samples_clicks} AS cclicks,
                      {cnt_samples_inj_keys} AS cinkeys,
                      {cnt_samples_inj_clicks} AS cinclks,
                      {cnt_moves} AS cmoves,
                      {cnt_switches} AS switches
               FROM events
               WHERE event_type IN ('keyboard','mouse','window')
                 AND timestamp >= '{start}' AND timestamp < '{end}'
               GROUP BY mi
             ),
             b AS (
               SELECT mi, 'input_keys' AS bk, keys AS s, ckeys AS c, maxid FROM m WHERE keys > 0
               UNION ALL
               SELECT mi, 'input_clicks', clicks, cclicks, maxid FROM m WHERE clicks > 0
               UNION ALL
               SELECT mi, 'input_moves', dist, cmoves, maxid FROM m WHERE dist > 0 OR cmoves > 0
               UNION ALL
               SELECT mi, 'window_switches', switches, switches, maxid FROM m WHERE switches > 0
               UNION ALL
               SELECT mi, 'input_keys_injected', ikeys, cinkeys, maxid FROM m WHERE ikeys > 0
               UNION ALL
               SELECT mi, 'input_clicks_injected', iclicks, cinclks, maxid FROM m WHERE iclicks > 0
             )
             SELECT '{date}', {hour}, mi, bk, s, c, maxid FROM b;",
            off = off,
            keys_row = crate::queries::KEYS_ROW_EXPR,
            clicks_row = crate::queries::CLICKS_ROW_EXPR,
            inj_keys_row = crate::queries::INJECTED_KEYS_ROW_EXPR,
            inj_clicks_row = crate::queries::INJECTED_CLICKS_ROW_EXPR,
            cnt_dist = CNT_DIST_EXPR,
            cnt_samples_keys = CNT_SAMPLES_KEYS_EXPR,
            cnt_samples_clicks = CNT_SAMPLES_CLICKS_EXPR,
            cnt_samples_inj_keys = CNT_SAMPLES_INJ_KEYS_EXPR,
            cnt_samples_inj_clicks = CNT_SAMPLES_INJ_CLICKS_EXPR,
            cnt_moves = CNT_MOVES_EXPR,
            cnt_switches = CNT_SWITCHES_EXPR,
            start = start,
            end = end,
            date = date,
            hour = hour,
        ))?;
        // 该日最后一小时：顺带重算该日 agg_daily app 行（小时 = 当日本地最大小时）。
        let max_hour: Option<i64> = conn
            .query_row(
                &format!(
                    "SELECT MAX(CAST(substr(datetime(timestamp, '{off}'), 12, 2) AS INTEGER)) \
                     FROM events WHERE event_type IN ('keyboard','mouse','window') \
                       AND timestamp >= '{dstart}' AND timestamp < '{dend}'",
                    off = off,
                    dstart = dstart,
                    dend = dend,
                ),
                [],
                |r| r.get(0),
            )
            .unwrap_or(None);
        if max_hour == Some(hour) {
            conn.execute(
                "DELETE FROM agg_daily WHERE date = ?1 AND bucket_id LIKE 'app:%'",
                params![date],
            )?;
            conn.execute_batch(&format!(
                "INSERT INTO agg_daily (date, bucket_id, sum_value, count_value)
                 SELECT substr(datetime(timestamp, '{off}'), 1, 10),
                        'app:' || app_name,
                        NULL,
                        COUNT(*)
                 FROM events
                 WHERE app_name IS NOT NULL AND app_name != '' AND event_type IN ('keyboard','mouse','window')
                   AND timestamp >= '{dstart}' AND timestamp < '{dend}'
                 GROUP BY 1, 2;",
                off = off,
                dstart = dstart,
                dend = dend,
            ))?;
        }

        // actor 证据缓存同块重算（与 agg_minute 同事务；列语义与增量
        // apply_actor_event 及 rebuild 路径逐字一致，同一组表达式宏）。
        conn.execute(
            "DELETE FROM agg_minute_actor WHERE date = ?1 AND hour = ?2",
            params![date, hour],
        )?;
        conn.execute_batch(&format!(
            "INSERT INTO agg_minute_actor (date, hour, minute, keys_total, keys_injected,
                                          clicks_total, clicks_injected, scroll_total, scroll_injected,
                                           max_event_rowid)
             WITH m AS (
               SELECT CAST(substr(datetime(timestamp, '{off}'), 15, 2) AS INTEGER) AS mi,
                      MAX(id) AS maxid,
                      SUM({keys_row}) AS keys,
                      SUM({clicks_row}) AS clicks,
                      SUM({inj_keys_row}) AS ikeys,
                      SUM({inj_clicks_row}) AS iclicks,
                      SUM({scroll_row}) AS scroll,
                      SUM({inj_scroll_row}) AS iscroll
               FROM events
               WHERE event_type IN ('keyboard','mouse')
                 AND timestamp >= '{start}' AND timestamp < '{end}'
               GROUP BY mi
             )
             SELECT '{date}', {hour}, mi, keys, ikeys, clicks, iclicks, scroll, iscroll, maxid FROM m
             WHERE keys > 0 OR ikeys > 0 OR clicks > 0 OR iclicks > 0 OR scroll > 0 OR iscroll > 0;",
            off = off,
            keys_row = crate::queries::KEYS_ROW_EXPR,
            clicks_row = crate::queries::CLICKS_ROW_EXPR,
            inj_keys_row = crate::queries::INJECTED_KEYS_ROW_EXPR,
            inj_clicks_row = crate::queries::INJECTED_CLICKS_ROW_EXPR,
            scroll_row = crate::queries::SCROLL_ROW_EXPR,
            inj_scroll_row = crate::queries::INJECTED_SCROLL_ROW_EXPR,
            start = start,
            end = end,
            date = date,
            hour = hour,
        ))?;
        Ok(())
    })();
    match r {
        Ok(()) => conn.execute_batch("COMMIT;")?,
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK;");
            return Err(e);
        }
    }
    Ok(())
}

/// 分块回填（可从 cursor 续跑；events 表扫过的块不再扫）。同步执行全部分块——
/// 供后台线程与测试使用。返回 agg_minute 行数。
pub fn backfill_all(conn: &Connection) -> crate::Result<usize> {
    let mut done = 0usize;
    loop {
        // 找 cursor 之后第一个有事件的本地 (date, hour) 块（sargable：按
        // idx_events_timestamp 范围探测每个本地小时；无事件的块廉价跳过）。
        let cursor = read_cursor(conn);
        // 本地日范围：events 的 UTC min/max 换本地日，向后兼容 cursor 起点跳过
        let (min_ts, max_ts): (String, String) = conn.query_row(
            "SELECT COALESCE(MIN(timestamp), ''), COALESCE(MAX(timestamp), '') FROM events",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        if max_ts.is_empty() {
            write_cursor(conn, BACKFILL_CURSOR_DONE);
            return Ok(count_agg_minute(conn));
        }
        let parse = |s: &str| -> Option<chrono::DateTime<Utc>> {
            chrono::DateTime::parse_from_rfc3339(s)
                .ok()
                .map(|t| t.with_timezone(&Utc))
        };
        let (Some(min_t), Some(max_t)) = (parse(&min_ts), parse(&max_ts)) else {
            write_cursor(conn, BACKFILL_CURSOR_DONE);
            return Ok(count_agg_minute(conn));
        };
        let min_local = min_t.with_timezone(&chrono::Local);
        let max_local = max_t.with_timezone(&chrono::Local);
        let mut next: Option<(String, i64)> = None;
        'outer: for day in min_local.date_naive().iter_days() {
            if day > max_local.date_naive() {
                break;
            }
            let ds = day.format("%Y-%m-%d").to_string();
            for h in 0..24i64 {
                if let Some(cur) = &cursor {
                    if cur.as_str() >= format!("{ds} {h:02}").as_str() {
                        continue;
                    }
                }
                if let Some((start, end)) = hour_bounds(&ds, h) {
                    let has: bool = conn
                        .query_row(
                            "SELECT EXISTS(SELECT 1 FROM events WHERE event_type IN ('keyboard','mouse','window') AND timestamp >= ?1 AND timestamp < ?2)",
                            params![start, end],
                            |r| r.get(0),
                        )
                        .unwrap_or(false);
                    if has {
                        next = Some((ds, h));
                        break 'outer;
                    }
                }
            }
        }
        let Some((ds, h)) = next else {
            write_cursor(conn, BACKFILL_CURSOR_DONE);
            return Ok(count_agg_minute(conn));
        };
        backfill_chunk(conn, &ds, h)?;
        write_cursor(conn, &format!("{ds} {h:02}"));
        done += 1;
        if done.is_multiple_of(24) {
            log::info!("agg 分块回填进度：{}（已完成 {} 块）", ds, done);
        }
    }
}

fn count_agg_minute(conn: &Connection) -> usize {
    conn.query_row("SELECT COUNT(*) FROM agg_minute", [], |r| {
        r.get::<_, i64>(0)
    })
    .unwrap_or(0) as usize
}

/// 某本地日期是否有**覆盖完整**的 agg_minute 缓存行。表不存在 / 查询失败一律
/// 返回 false（调用方回退 events 现算——保证聚合缺失时行为退化为原始慢路径
/// 而非错误）。
///
/// 审查 33-F5：缓存命中加廉价完整性校验——该日「会产生聚合行的 input_agg
/// 分钟数」（keys/clicks/moves/move_distance_px 任一 > 0 的分钟；纯 scroll
/// 分钟不产聚合行，排除在外）超过该日**去重后的 agg_minute 分钟数**即视为
/// 残缺（部分旧聚合行残留的中间态），回退 events 现算，不再漏算缺失分钟。
/// 两边都必须按「分钟」去重计数：agg_minute 主键是 (date, hour, minute,
/// bucket_id)，同一分钟最多可产 4 行（input_keys/clicks/moves/
/// window_switches），若拿行数跟分钟数比，上午完整聚合的行数放大（至多
/// 4×分钟数）可以盖过全天 input_agg 分钟数，把「下午缺失」误判成覆盖完整。
/// 去重后完整状态下每个这样的分钟至少对应一个 agg_minute 分钟，不会误报。
pub fn has_minute_for_date(conn: &Connection, date: &str) -> bool {
    let bounds = crate::queries::local_day_range(date);
    let sql = match &bounds {
        Some((_start, _end)) => format!(
            "SELECT EXISTS(SELECT 1 FROM agg_minute WHERE date = ?1)
             AND NOT (
               (SELECT COUNT(DISTINCT substr(datetime(timestamp, '{off}'), 1, 16))
                FROM events
                WHERE event_action = 'input_agg'
                  AND timestamp >= ?2 AND timestamp < ?3
                  AND json_valid(event_data)
                  AND (COALESCE(json_extract(event_data, '$.keys'), 0) > 0
                    OR COALESCE(json_extract(event_data, '$.clicks'), 0) > 0
                    OR COALESCE(json_extract(event_data, '$.moves'), 0) > 0
                    OR COALESCE(json_extract(event_data, '$.move_distance_px'), 0) > 0))
               > (SELECT COUNT(DISTINCT hour * 60 + minute) FROM agg_minute
                   WHERE date = ?1
                     AND bucket_id IN ('input_keys','input_clicks','input_moves'))
             )",
            off = crate::queries::LOCAL_MODIFIER_AT_EVENT,
        ),
        None => {
            // 日界不可解析（异常日期串）：退回旧的纯 EXISTS 判定
            return conn
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM agg_minute WHERE date = ?1)",
                    params![date],
                    |r| r.get(0),
                )
                .unwrap_or(false);
        }
    };
    let (start, end) = bounds.unwrap();
    conn.query_row(&sql, params![date, start, end], |r| r.get(0))
        .unwrap_or(false)
}

/// [`has_minute_for_date`] 的批量版：逐本地日「agg_minute 缓存完备」标记。
///
/// 异常多日检测路径（`detect_all_days_with_bridge`）此前逐日检测器各调
/// [`has_minute_for_date`]（深夜 / APM 突增 / 注入标注共 3 次/日），每次都对
/// 当日 events input_agg 行做 COUNT DISTINCT。本函数把 N 日 × 3 次探测换成
/// 两条整窗查询：agg_minute 逐日去重分钟数（GROUP BY date）+ events 输入分钟
/// 全窗按本地日分组（单次区间扫描），逐日语义与单点探测严格等价
///（「该日有 agg 行」且「当日输入分钟数 ≤ agg 去重分钟数」）。
///
/// 失败口径与单点探测一致：agg_minute 表缺失 / 查询失败 → 全 false（调用方
/// 回退 events 现算慢路径）。`dates` 取有效本地日期（调用方传
/// `date_offset_str` 结果）；日界不可解析的日期沿用单点探测的纯 EXISTS
/// 口径（不做完整性校验）。返回与 `dates` 同序。
pub fn minutes_complete_for_dates(conn: &Connection, dates: &[String]) -> Vec<bool> {
    let n = dates.len();
    if n == 0 {
        return Vec::new();
    }
    // agg_minute 侧：逐日去重分钟数（主键含 bucket_id，同一分钟至多 4 行，
    // 与单点探测同口径按 hour*60+minute 去重）。审查修复（联合审查 P2-1 /
    // has_minute 假完备）：**只数输入桶**——窗口切换密集日若把 window_switches
    // 行计入，agg 侧分钟数可被「垫高」，把输入桶残缺的日期误判成覆盖完整，
    // 缓存读路径整日漏算输入。与单点探测同谓词过滤。
    let placeholders: Vec<String> = (1..=n).map(|i| format!("?{i}")).collect();
    let sql_agg = format!(
        "SELECT date, COUNT(DISTINCT hour * 60 + minute) \
         FROM agg_minute WHERE date IN ({placeholders}) \
           AND bucket_id IN ('input_keys','input_clicks','input_moves') \
         GROUP BY date",
        placeholders = placeholders.join(", ")
    );
    let mut agg_min: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
    if let Ok(mut stmt) = conn.prepare(&sql_agg) {
        if let Ok(rows) = stmt.query_map(
            rusqlite::params_from_iter(dates.iter().map(|d| d.as_str())),
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
        ) {
            for (d, c) in rows.flatten() {
                agg_min.insert(d, c);
            }
        }
    }
    // 表缺失 / 无 agg 行 / 查询失败 → 全 false（回退 events 现算，同单点口径）
    if agg_min.is_empty() {
        return vec![false; n];
    }
    // events 侧：整窗输入分钟按本地日去重计数（输入分钟口径与单点探测的
    // 完整性校验一致：keys/clicks/moves/move_distance_px 任一 > 0）
    let mut lo: Option<String> = None;
    let mut hi: Option<String> = None;
    let mut parseable = vec![false; n];
    for (i, d) in dates.iter().enumerate() {
        if let Some((s, e)) = crate::queries::local_day_range(d) {
            parseable[i] = true;
            lo = Some(match lo {
                Some(l) if l <= s => l,
                _ => s,
            });
            hi = Some(match hi {
                Some(h) if h >= e => h,
                _ => e,
            });
        }
    }
    let mut events_min: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
    if let (Some(lo), Some(hi)) = (lo, hi) {
        let off = crate::queries::LOCAL_MODIFIER_AT_EVENT;
        let sql_events = format!(
            "SELECT substr(datetime(timestamp, '{off}'), 1, 10) AS d, \
             COUNT(DISTINCT substr(datetime(timestamp, '{off}'), 1, 16)) \
             FROM events \
             WHERE event_action = 'input_agg' \
               AND json_valid(event_data) \
               AND (COALESCE(json_extract(event_data, '$.keys'), 0) > 0 \
                 OR COALESCE(json_extract(event_data, '$.clicks'), 0) > 0 \
                 OR COALESCE(json_extract(event_data, '$.moves'), 0) > 0 \
                 OR COALESCE(json_extract(event_data, '$.move_distance_px'), 0) > 0) \
               AND timestamp >= ?1 AND timestamp < ?2 \
             GROUP BY d",
        );
        if let Ok(mut stmt) = conn.prepare(&sql_events) {
            if let Ok(rows) = stmt.query_map(params![&lo, &hi], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
            }) {
                for (d, c) in rows.flatten() {
                    events_min.insert(d, c);
                }
            }
        }
    }
    dates
        .iter()
        .enumerate()
        .map(|(i, d)| {
            if !parseable[i] {
                // 日界不可解析：单点探测回退纯 EXISTS——只看该日有无 agg 行
                agg_min.contains_key(d)
            } else {
                match agg_min.get(d) {
                    Some(agg_c) => events_min.get(d).copied().unwrap_or(0) <= *agg_c,
                    None => false,
                }
            }
        })
        .collect()
}

/// agg_daily 是否有 per-app 缓存行（可选限定某本地日期）。
pub fn has_app_daily(conn: &Connection, date: Option<&str>) -> bool {
    match date {
        Some(d) => conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM agg_daily WHERE date = ?1 AND bucket_id LIKE 'app:%')",
                params![d],
                |r| r.get(0),
            )
            .unwrap_or(false),
        None => conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM agg_daily WHERE bucket_id LIKE 'app:%')",
                [],
                |r| r.get(0),
            )
            .unwrap_or(false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(crate::db::SCHEMA).unwrap();
        let _ = crate::db::run_migrations(&c);
        c
    }

    fn ins(conn: &Connection, ts: &str, t: &str, a: &str, app: Option<&str>, data: Option<&str>) {
        conn.execute(
            "INSERT INTO events (timestamp, event_type, event_action, event_data, app_name, window_title, session_id)
             VALUES (?1,?2,?3,?4,?5,?6,1)",
            params![ts, t, a, data, app, app],
        )
        .unwrap();
    }

    /// UTC+8 固定偏移下"本地 2026-06-15 10:30"对应的 UTC RFC3339。
    /// 测试不依赖运行机器时区：datetime(ts,'+08:00 seconds') 是 SQL 端换算，
    /// local_offset_modifier 取运行机器——因此这里改用 UTC 机器无关断言：
    /// 直接断言重建后按本地日的总量正确，见 raw_preserved 与 minute_buckets。
    fn any_local_minute_row(conn: &Connection, bucket: &str) -> i64 {
        conn.query_row(
            "SELECT CAST(COALESCE(SUM(sum_value),0) AS INTEGER) FROM agg_minute WHERE bucket_id=?1",
            params![bucket],
            |r| r.get(0),
        )
        .unwrap()
    }

    #[test]
    fn rebuild_buckets_and_app_rows() {
        let c = conn();
        ins(
            &c,
            "2026-06-15T02:30:00+00:00",
            "keyboard",
            "press",
            None,
            None,
        );
        ins(
            &c,
            "2026-06-15T02:30:30+00:00",
            "keyboard",
            "press",
            None,
            None,
        );
        ins(
            &c,
            "2026-06-15T02:30:40+00:00",
            "mouse",
            "click",
            None,
            None,
        );
        ins(
            &c,
            "2026-06-15T02:31:00+00:00",
            "window",
            "switch",
            Some("code.exe"),
            None,
        );
        ins(
            &c,
            "2026-06-15T02:32:00+00:00",
            "keyboard",
            "input_agg",
            None,
            Some(r#"{"keys":7,"samples":7}"#),
        );

        let n = rebuild_all(&c).unwrap();
        assert!(n >= 3);
        // 总量与形态无关：2 条 raw press + 1 条 agg 行(7 keys)
        assert_eq!(any_local_minute_row(&c, "input_keys"), 9);
        assert_eq!(any_local_minute_row(&c, "input_clicks"), 1);
        assert_eq!(any_local_minute_row(&c, "window_switches"), 1);
        // app 日聚合：window 行带 app
        let app_cnt: i64 = c
            .query_row(
                "SELECT COALESCE(SUM(count_value),0) FROM agg_daily WHERE bucket_id='app:code.exe'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(app_cnt, 1);
        // 幂等：重建两次结果一致
        rebuild_all(&c).unwrap();
        assert_eq!(any_local_minute_row(&c, "input_keys"), 9);
    }

    /// 原始数据神圣性：重建/回填绝不改动 events 行（数量 + 内容全等）。
    #[test]
    fn raw_events_untouched_by_rebuild_and_backfill() {
        let c = conn();
        ins(
            &c,
            "2026-06-15T02:30:00+00:00",
            "keyboard",
            "press",
            None,
            None,
        );
        ins(
            &c,
            "2026-06-15T02:30:30+00:00",
            "mouse",
            "click",
            None,
            None,
        );
        ins(
            &c,
            "2026-06-15T03:00:00+00:00",
            "window",
            "switch",
            Some("word.exe"),
            Some(r#"{"hwnd":1}"#),
        );

        let snapshot = |c: &Connection| -> (i64, String) {
            let n: i64 = c
                .query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))
                .unwrap();
            // 内容指纹：全行拼接（含 id 顺序）
            let mut stmt = c
                .prepare("SELECT id||'|'||timestamp||'|'||event_type||'|'||event_action||'|'||COALESCE(event_data,'')||'|'||COALESCE(app_name,'')||'|'||COALESCE(window_title,'')||'|'||COALESCE(session_id,-1) FROM events ORDER BY id")
                .unwrap();
            let hash: String = stmt
                .query_map([], |r| r.get::<_, String>(0))
                .unwrap()
                .filter_map(|r| r.ok())
                .collect::<Vec<_>>()
                .join(";");
            (n, hash)
        };

        let before = snapshot(&c);
        assert!(backfill_if_needed(&c), "应有回填发生");
        rebuild_all(&c).unwrap();
        rebuild_all(&c).unwrap();
        let after = snapshot(&c);
        assert_eq!(before, after, "events 行数量与内容必须完全不变");
    }

    #[test]
    fn backfill_skipped_when_agg_present_or_events_empty() {
        let empty = conn();
        assert!(!backfill_if_needed(&empty), "空库不回填");

        let c = conn();
        ins(
            &c,
            "2026-06-15T02:30:00+00:00",
            "keyboard",
            "press",
            None,
            None,
        );
        assert!(backfill_if_needed(&c));
        // 已有 agg → 不再回填（增量维护负责新鲜度）
        assert!(!backfill_if_needed(&c));
    }

    #[test]
    fn incremental_update_matches_rebuild() {
        let c = conn();
        let e_press = Event::new(EventAction::Press, EventType::Keyboard);
        let mut e = e_press.clone();
        e.timestamp = "2026-06-15T02:30:10+00:00".into();
        let mut e2 = Event::new(EventAction::Switch, EventType::Window).app("code", "t");
        e2.timestamp = "2026-06-15T02:30:40+00:00".into();
        let mut e3 = Event::new(EventAction::InputAgg, EventType::Keyboard)
            .data(serde_json::json!({"keys": 5, "samples": 5}));
        e3.timestamp = "2026-06-15T02:31:00+00:00".into();

        for (i, ev) in [&e, &e2, &e3].iter().enumerate() {
            apply_event(&c, ev, i as i64 + 1).unwrap();
            ins(
                &c,
                &ev.timestamp,
                ev.event_type.as_str(),
                ev.event_action.as_str(),
                ev.app_name.as_deref(),
                ev.event_data.as_ref().map(|v| v.to_string()).as_deref(),
            );
        }
        let incremental = any_local_minute_row(&c, "input_keys");

        rebuild_all(&c).unwrap();
        let rebuilt = any_local_minute_row(&c, "input_keys");
        assert_eq!(incremental, rebuilt, "增量维护必须与全量重建等价");
        assert_eq!(incremental, 6);
    }

    /// 联合审查 P1-1 回归：app 日桶增量与重建同谓词——带 app_name 的非
    /// switch 行（browser 的 tab_change）此前只在重建路径计入，增量写出的
    /// agg_daily 在 heal/backfill 重写当日后计数跳变、new_app_surge 基线
    /// 失真（同一指标三读路径两种答案）。统一后增量/重建必须逐行一致。
    #[test]
    fn app_daily_incremental_matches_rebuild_with_tab_change() {
        let c = conn();
        let mk = |action: EventAction, app: &str, ts: &str| {
            let mut e = Event::new(action, EventType::Window).app(app, "t");
            e.timestamp = ts.into();
            e
        };
        let empty_agg = {
            let mut e = Event::new(EventAction::InputAgg, EventType::Keyboard)
                .data(serde_json::json!({"keys": 5, "samples": 5}))
                .app("", "");
            e.timestamp = "2026-06-15T02:30:40+00:00".into();
            e
        };
        let events = [
            mk(EventAction::Switch, "code", "2026-06-15T02:30:10+00:00"),
            mk(
                EventAction::TabChange,
                "chrome",
                "2026-06-15T02:30:20+00:00",
            ),
            mk(
                EventAction::TabChange,
                "chrome",
                "2026-06-15T02:30:30+00:00",
            ),
            empty_agg,
        ];
        for (i, ev) in events.iter().enumerate() {
            apply_event(&c, ev, i as i64 + 1).unwrap();
            ins(
                &c,
                &ev.timestamp,
                ev.event_type.as_str(),
                ev.event_action.as_str(),
                ev.app_name.as_deref(),
                ev.event_data.as_ref().map(|v| v.to_string()).as_deref(),
            );
        }
        let app_rows = |c: &Connection| -> Vec<(String, i64)> {
            let mut stmt = c
                .prepare(
                    "SELECT substr(bucket_id,5), COALESCE(count_value,0) \
                     FROM agg_daily WHERE bucket_id LIKE 'app:%' ORDER BY 1",
                )
                .unwrap();
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .unwrap()
                .flatten()
                .collect()
        };
        let incremental = app_rows(&c);
        assert_eq!(
            incremental,
            vec![("chrome".into(), 2), ("code".into(), 1)],
            "增量必须计入 tab_change（与重建同谓词），空名 input_agg 行不得计入"
        );

        rebuild_all(&c).unwrap();
        let rebuilt = app_rows(&c);
        assert_eq!(
            incremental, rebuilt,
            "app 日桶增量必须与全量重建等价（tab_change 不再分叉）"
        );
    }

    /// 审查 P1 回归：mouse input_agg 行同时带 samples 与 moves 时，
    /// input_moves 的 count 必须是 $.moves 而非 $.samples，
    /// 且增量维护与 rebuild_all / backfill_all 逐行一致。
    #[test]
    fn moves_count_uses_moves_field_not_samples() {
        let c = conn();
        // 事件 JSON：clicks/samples/moves/move_distance_px 并存
        let e = Event::new(EventAction::InputAgg, EventType::Mouse).data(serde_json::json!({
            "clicks": 3, "samples": 20, "moves": 10, "move_distance_px": 500,
        }));
        let mut e = e;
        e.timestamp = "2026-06-15T02:30:00+00:00".into();
        ins(
            &c,
            &e.timestamp,
            "mouse",
            "input_agg",
            None,
            Some(r#"{"clicks":3,"samples":20,"moves":10,"move_distance_px":500}"#),
        );
        apply_event(&c, &e, 1).unwrap();

        // 增量结果：input_moves sum=500 count=10（不是 20）
        let (sum, cnt): (f64, i64) = c
            .query_row(
                "SELECT COALESCE(sum_value,0), COALESCE(count_value,0) FROM agg_minute WHERE bucket_id='input_moves'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((sum as i64, cnt), (500, 10), "count 必须取 $.moves");
        // input_clicks 的 count 仍是 samples
        let ccnt: i64 = c
            .query_row(
                "SELECT COALESCE(count_value,0) FROM agg_minute WHERE bucket_id='input_clicks'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(ccnt, 20);

        let fingerprint = |c: &Connection| -> String {
            let mut stmt = c
                .prepare(
                    "SELECT bucket_id||'|'||COALESCE(sum_value,0)||'|'||COALESCE(count_value,0)
                     FROM agg_minute ORDER BY bucket_id",
                )
                .unwrap();
            stmt.query_map([], |r| r.get::<_, String>(0))
                .unwrap()
                .filter_map(|r| r.ok())
                .collect::<Vec<_>>()
                .join(";")
        };
        let incremental_fp = fingerprint(&c);

        rebuild_all(&c).unwrap();
        assert_eq!(
            incremental_fp,
            fingerprint(&c),
            "samples/moves 并存时增量与 rebuild 必须逐行一致"
        );
        c.execute("DELETE FROM agg_minute", []).unwrap();
        backfill_all(&c).unwrap();
        assert_eq!(
            incremental_fp,
            fingerprint(&c),
            "samples/moves 并存时增量与 backfill 必须逐行一致"
        );
    }

    /// 审查 P2 回归：max_event_rowid 幂等防护——同事件重复投递（如分块回填
    /// 重算已包含该事件后迟到的增量 upsert）不得重复累计。
    #[test]
    fn incremental_upsert_is_idempotent_via_max_event_rowid() {
        let c = conn();
        let e = Event::new(EventAction::Press, EventType::Keyboard);
        let mut e = e;
        e.timestamp = "2026-06-15T02:30:10+00:00".into();
        ins(&c, &e.timestamp, "keyboard", "press", None, None);
        apply_event(&c, &e, 1).unwrap();
        // 同一事件（rowid 1）再次投递：被守卫跳过
        apply_event(&c, &e, 1).unwrap();
        let cnt: i64 = c
            .query_row(
                "SELECT COALESCE(count_value,0) FROM agg_minute WHERE bucket_id='input_keys'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(cnt, 1, "同事件重复投递不重复累计");

        // backfill 重算（包含该事件）后，迟到增量（rowid 1）也不重复
        backfill_all(&c).unwrap();
        let rebuilt: i64 = c
            .query_row(
                "SELECT COALESCE(count_value,0) FROM agg_minute WHERE bucket_id='input_keys'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(rebuilt, 1);
        apply_event(&c, &e, 1).unwrap();
        let after: i64 = c
            .query_row(
                "SELECT COALESCE(count_value,0) FROM agg_minute WHERE bucket_id='input_keys'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(after, 1, "重算后的迟到增量被 max_event_rowid 守卫跳过");

        // 新事件（rowid 更大）正常累计
        let e2 = Event::new(EventAction::Press, EventType::Keyboard);
        let mut e2 = e2;
        e2.timestamp = "2026-06-15T02:30:20+00:00".into();
        ins(&c, &e2.timestamp, "keyboard", "press", None, None);
        apply_event(&c, &e2, 2).unwrap();
        let after2: i64 = c
            .query_row(
                "SELECT COALESCE(count_value,0) FROM agg_minute WHERE bucket_id='input_keys'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(after2, 2);
    }

    /// 交叉审查 P3：欠聚合核对只看最近 7 天（自愈窗口）。8 天前遗留的欠聚合
    /// 日期不出现在核对结果里（历史遗留需显式 rebuild_agg，见函数 doc 取舍说明）。
    #[test]
    fn under_agg_dates_only_checks_recent_seven_days() {
        let c = conn();
        // 8 天前的 press（欠聚合，超出 7 天窗口）+ 今天的 press（欠聚合，窗口内）
        let old = (Utc::now() - chrono::Duration::days(8)).to_rfc3339();
        let recent = Utc::now().to_rfc3339();
        ins(&c, &old, "keyboard", "press", None, None);
        ins(&c, &recent, "keyboard", "press", None, None);

        let dates = under_agg_dates(&c);
        let old_local_date = (Utc::now() - chrono::Duration::days(8))
            .with_timezone(&Local)
            .format("%Y-%m-%d")
            .to_string();
        assert!(
            !dates.contains(&old_local_date),
            "8 天前的欠聚合日期不应触发启动自愈（窗口 7 天）: {dates:?}"
        );
        assert_eq!(dates.len(), 1, "窗口内的今天仍应被核对出欠聚合: {dates:?}");
    }

    // === 混合分钟一致性：同一本地分钟混含 raw 与 input_agg 两形态 ===
    // 触发面：tray 按 input_counts_only 切换重启采集器，分钟中途切换即产生
    // 混合分钟。既有等价测试（incremental_update_matches_rebuild 等）把两形态
    // 放在不同分钟，覆盖不到该边界；本模块专测同分钟混合。
    mod mixed_minute_parity {
        use super::*;

        /// 契约（模块头「定位」：增量 apply_event 与全量 rebuild_all 必须逐行
        /// 一致）在混合分钟上成立。统一口径为事件 SUM（与今日按键读 events
        /// SUM 的消费面一致）：raw press 每行 1 + input_agg $.keys 快照同桶
        /// 求和；MAX 快照式增量对同分钟两形态取 MAX 而非 SUM，必然分叉。
        #[test]
        fn input_keys_bucket_matches_rebuild_in_mixed_minute() {
            let c = conn();
            // 前半分钟：raw press 形态 5 行
            let mut events: Vec<Event> = Vec::new();
            for sec in [10u32, 15, 20, 25, 30] {
                let mut e = Event::new(EventAction::Press, EventType::Keyboard);
                e.timestamp = format!("2026-06-15T02:30:{sec:02}+00:00");
                events.push(e);
            }
            // 同分钟后半：采集器切到计数模式，input_agg 快照只覆盖本段 keys=2
            let mut agg = Event::new(EventAction::InputAgg, EventType::Keyboard)
                .data(serde_json::json!({"keys": 2, "samples": 2}));
            agg.timestamp = "2026-06-15T02:30:50+00:00".into();
            events.push(agg);

            // 路径 A：增量——与生产 writer 同事务配对顺序一致
            // （insert_events_with_agg_tx：先落库取 rowid，后 apply_event；
            // 混合分钟重算依赖本行已入 events）
            for (i, ev) in events.iter().enumerate() {
                ins(
                    &c,
                    &ev.timestamp,
                    ev.event_type.as_str(),
                    ev.event_action.as_str(),
                    ev.app_name.as_deref(),
                    ev.event_data.as_ref().map(|v| v.to_string()).as_deref(),
                );
                apply_event(&c, ev, i as i64 + 1).unwrap();
            }
            let incremental = any_local_minute_row(&c, "input_keys");

            // 路径 B：同库同数据全量重建
            rebuild_all(&c).unwrap();
            let rebuilt = any_local_minute_row(&c, "input_keys");
            assert_eq!(
                incremental, rebuilt,
                "混合分钟（同分钟 raw+input_agg）增量必须与全量重建等价"
            );
            // 钉值：统一口径为事件 SUM = 5(raw) + 2(agg 快照) = 7；
            // MAX 快照式增量给 MAX(5,2)=5，与消费面（events SUM）异数
            assert_eq!(
                rebuilt, 7,
                "统一口径为事件 SUM：5 条 raw press + input_agg keys=2"
            );
        }
    }
}

#[cfg(test)]
mod perf3_tests {
    use super::*;

    fn conn() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(crate::db::SCHEMA).unwrap();
        let _ = crate::db::run_migrations(&c);
        c
    }

    fn ins(conn: &Connection, ts: &str, t: &str, a: &str, app: Option<&str>, data: Option<&str>) {
        conn.execute(
            "INSERT INTO events (timestamp, event_type, event_action, event_data, app_name, window_title, session_id)
             VALUES (?1,?2,?3,?4,?5,?6,1)",
            params![ts, t, a, data, app, app],
        )
        .unwrap();
    }

    fn fingerprint(conn: &Connection) -> String {
        let mut stmt = conn
            .prepare(
                "SELECT date||'|'||hour||'|'||minute||'|'||bucket_id||'|'||COALESCE(sum_value,0)||'|'||COALESCE(count_value,0)
                 FROM agg_minute ORDER BY date, hour, minute, bucket_id",
            )
            .unwrap();
        stmt.query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect::<Vec<_>>()
            .join(";")
    }

    /// perf3：分块回填必须与全量 rebuild_all 完全等价（跨日 + raw/input_agg 混合形态）。
    #[test]
    fn chunked_backfill_equals_full_rebuild() {
        let c = conn();
        ins(
            &c,
            "2026-06-14T18:59:59+00:00",
            "keyboard",
            "press",
            None,
            None,
        );
        ins(
            &c,
            "2026-06-14T19:00:10+00:00",
            "mouse",
            "click",
            None,
            None,
        );
        ins(
            &c,
            "2026-06-14T19:00:30+00:00",
            "window",
            "switch",
            Some("code.exe"),
            None,
        );
        ins(
            &c,
            "2026-06-15T02:30:10+00:00",
            "keyboard",
            "press",
            None,
            None,
        );
        ins(
            &c,
            "2026-06-15T02:31:00+00:00",
            "keyboard",
            "input_agg",
            None,
            Some(r#"{"keys":7,"samples":7}"#),
        );
        ins(
            &c,
            "2026-06-15T03:00:00+00:00",
            "mouse",
            "move",
            Some(""),
            Some(r#"{"x":1,"y":2}"#),
        );

        backfill_all(&c).unwrap();
        let chunked_fp = fingerprint(&c);
        let chunked_daily: Vec<(String, i64)> = {
            let mut stmt = c
                .prepare(
                    "SELECT bucket_id, COALESCE(count_value,0) FROM agg_daily ORDER BY bucket_id",
                )
                .unwrap();
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .unwrap()
                .filter_map(|r| r.ok())
                .collect()
        };

        rebuild_all(&c).unwrap();
        assert_eq!(
            chunked_fp,
            fingerprint(&c),
            "分块回填与全量重建必须逐行一致"
        );
        rebuild_all(&c).unwrap();
        let daily: Vec<(String, i64)> = {
            let mut stmt = c
                .prepare(
                    "SELECT bucket_id, COALESCE(count_value,0) FROM agg_daily ORDER BY bucket_id",
                )
                .unwrap();
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .unwrap()
                .filter_map(|r| r.ok())
                .collect()
        };
        assert_eq!(chunked_daily, daily, "agg_daily 分块回填与全量重建一致");
        assert!(!read_cursor_incomplete(&c), "完成后 cursor 应为 done");
    }

    /// perf3：并发交错——分块回填期间 writer 的增量 upsert 不能造成丢失或重复。
    /// 模拟：回填某块后，增量 upsert 追加事件；再回填下一块（重扫同日）
    /// 不得破坏已完成块的计数。
    #[test]
    fn chunked_backfill_survives_interleaved_incremental_upserts() {
        let c = conn();
        ins(
            &c,
            "2026-06-15T02:30:10+00:00",
            "keyboard",
            "press",
            None,
            None,
        );
        backfill_all(&c).unwrap();

        // writer 路径：新事件落库 + 增量 agg（与 collector flush 相同顺序）
        let mut e = Event::new(EventAction::Press, EventType::Keyboard);
        e.timestamp = "2026-06-15T02:31:20+00:00".into();
        ins(&c, &e.timestamp, "keyboard", "press", None, None);
        apply_event(&c, &e, 2).unwrap();

        // 已完成块计数 = 重建结果
        rebuild_all(&c).unwrap();
        let rebuilt = fingerprint(&c);
        // 注意：incremental 已把 02:31 的 keys=1 加上；rebuild 也算同一条 → 相等

        // 分块回填（cursor=done 会跳过 → 清 cursor 强制重跑，等价于"中断续跑"）
        c.execute("DELETE FROM metadata WHERE key='agg_backfill_cursor'", [])
            .unwrap();
        c.execute("DELETE FROM agg_minute", []).unwrap();
        c.execute("DELETE FROM agg_daily WHERE bucket_id LIKE 'app:%'", [])
            .unwrap();
        backfill_all(&c).unwrap();
        assert_eq!(
            fingerprint(&c),
            rebuilt,
            "交错增量后重跑分块回填仍与重建一致"
        );
    }
}
