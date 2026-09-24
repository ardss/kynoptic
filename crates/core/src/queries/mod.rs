//! 共享查询函数
//!
//! 消除 commands.rs / snapshot.rs / tray.rs 中重复的 SQL 查询。
//!
//! 容错策略：聚合类查询（COUNT/SUM）在底层失败时返回 0/空，但**会记录 log**，
//! 区分"无数据"与"查询失败"——之前用 `unwrap_or(0)` 静默吞错，调试时无法
//! 发现 SQL 异常。注意 `QueryReturnedNoRows` 不算失败（COUNT 永不返回，但
//! current_app 这类 LIMIT 1 查询在无匹配时是正常的空结果）。
//!
//! **强类型约定**：所有按 `(event_type, event_action)` 维度查询的 API 一律用
//! [`crate::types::EventType`] / [`crate::types::EventAction`] 枚举绑定参数（内部
//! 调 `as_str()`）。无字符串 WHERE 拼接的 fallback——枚举改名时编译器强制跟随，
//! 避免静默失效。
//!
//! # 模块组织
//!
//! 本模块物理拆分为若干子模块，但通过 `pub use` 在 [`queries`] 根 re-export
//! 全部公开项，故外部调用路径（`queries::xxx`）与拆分前完全一致：
//! - [`summary`]：今日/全量聚合计数（keys/clicks/active_min/sessions 等）
//! - [`charts`]：图表数据 + 当前值类查询（top_apps/hourly/minute/daily/breakdown/current_*）
//! - [`history`]：事件历史快照（event_history/latest_event_ts_*/recent_event_data_*）
//! - [`maintenance`]：DB 维护统计与清理（ctl db stats/cleanup/export 用）

use chrono::{Local, Utc};
use rusqlite::Row;

mod charts;
mod history;
mod maintenance;
mod presence;
mod summary;

// ─── 共享 helper（子模块通过 super:: 引用） ──────────────────────────────────

// 审查 HIGH（DST 修复）遗留说明：旧的 local_offset_seconds /
// local_offset_modifier（取"当前时刻"的固定偏移）已被
// [`LOCAL_MODIFIER_AT_EVENT`]（SQLite 'localtime'，按事件时刻取历史时区
// 规则）取代并删除——固定偏移会让跨 DST 的历史日小时桶整体错位 1 小时。

/// 聚合/投影用的"事件时刻本地化"修饰符（审查 HIGH：DST 修复）。
///
/// `localtime` 让 SQLite 按每行事件所属时刻套用操作系统的历史时区规则
/// （与活写路径 `agg::apply_event` 的 chrono Local 换算同口径）；而
/// [`local_offset_modifier`] 固定用**当前时刻**的偏移，跨 DST 的历史日会在
/// 小时桶上整体错位 1 小时并可跨日溢出。仅用于 SELECT/GROUP BY 投影，
/// WHERE 仍走可下推索引的 timestamp 裸区间。
pub const LOCAL_MODIFIER_AT_EVENT: &str = "localtime";

/// 计数类查询的默认值（失败时）。
/// 真正的 SQL 异常会 log warn；`QueryReturnedNoRows` 静默（视为无数据）。
pub(crate) fn count_or_log(res: rusqlite::Result<i64>, ctx: &str) -> i64 {
    res.unwrap_or_else(|e| {
        if !matches!(e, rusqlite::Error::QueryReturnedNoRows) {
            log::warn!("查询失败({ctx}): {e}");
        }
        0
    })
}

/// 字符串类查询的默认值（失败时）。
pub(crate) fn string_or_log(res: rusqlite::Result<String>, ctx: &str) -> String {
    res.unwrap_or_else(|e| {
        if !matches!(e, rusqlite::Error::QueryReturnedNoRows) {
            log::warn!("查询失败({ctx}): {e}");
        }
        String::new()
    })
}

/// 行流收集（审查修复 2026-09，"不炸不丢"）：替代 `rows.flatten()` 的静默
/// 丢行——未来版本写入的异型行（如 session_id 列被写入 TEXT）会让
/// query_map 逐行返回 Err(InvalidColumnType)，flatten 无声丢弃后命令仍报
/// 成功、行数与 db stats 自相矛盾。本 helper 累计跳过行数并 log::warn
/// （含首条错误），保证丢行留痕。
pub(crate) fn collect_rows_warn<T, I>(rows: I, ctx: &str) -> Vec<T>
where
    I: Iterator<Item = rusqlite::Result<T>>,
{
    let mut out = Vec::new();
    let mut skipped = 0usize;
    let mut first_err: Option<rusqlite::Error> = None;
    for r in rows {
        match r {
            Ok(v) => out.push(v),
            Err(e) => {
                skipped += 1;
                if first_err.is_none() {
                    first_err = Some(e);
                }
            }
        }
    }
    if skipped > 0 {
        log::warn!(
            "查询 {ctx} 有 {skipped} 行因类型不兼容被跳过（首条: {}）",
            first_err.map(|e| e.to_string()).unwrap_or_default()
        );
    }
    out
}

pub fn get_count_i64(row: &Row<'_>) -> rusqlite::Result<i64> {
    row.get(0)
}

pub fn get_string(row: &Row<'_>) -> rusqlite::Result<String> {
    row.get(0)
}

/// 本地钟面时间 → UTC（供「本地日始/用户输入的本地钟面时间」换算共用）。
///
/// DST 处理：
/// - 唯一映射：直接换算；
/// - 歧义（秋拨重复小时）：取最早者（与旧 `.earliest()` 行为一致）；
/// - 空洞（春拨本地钟面不存在，如智利/黎巴嫩春推日的午夜）：顺延到下一个
///   有效时刻（逐小时步进，最多 49 次封顶）——此前兜底把 naive 钟面直接当
///   UTC 解释，日起点整体错位一个本地偏移量。
pub fn local_naive_to_utc(naive: chrono::NaiveDateTime) -> Option<chrono::DateTime<Utc>> {
    use chrono::TimeZone;
    let mut cur = naive;
    for _ in 0..49 {
        match Local.from_local_datetime(&cur) {
            chrono::LocalResult::Single(t) => return Some(t.with_timezone(&Utc)),
            chrono::LocalResult::Ambiguous(a, _) => {
                return Some(a.with_timezone(&Utc));
            }
            chrono::LocalResult::None => cur += chrono::Duration::hours(1),
        }
    }
    // 理论不可达（49 小时内必有有效映射）；兜底返回 None（调用方按无窗口处理）
    None
}

/// 将「本地某一天」`date`（`YYYY-MM-DD`，用户钟表上的日期）转成
/// `[start, end)` 的 UTC RFC3339 边界，供 `WHERE timestamp >= start AND timestamp < end`。
///
/// 与 [`today_range`] 同源：所有"按天"查询都应走这里，确保**实时面板的今日**与
/// **分析面板的某天**对"天"的定义一致（均为用户本地时区的自然日），而非 UTC 日。
///
/// `date` 解析失败时返回 `None`，调用方应回退到原始字符串前缀匹配或视为空。
pub fn local_day_range(date: &str) -> Option<(String, String)> {
    use chrono::NaiveDate;
    let day = NaiveDate::parse_from_str(date, "%Y-%m-%d").ok()?;
    let to_utc_rfc3339 = |d: NaiveDate| -> Option<String> {
        let naive = d.and_hms_opt(0, 0, 0)?;
        local_naive_to_utc(naive).map(|dt| dt.to_rfc3339())
    };
    let start = to_utc_rfc3339(day)?;
    let end = to_utc_rfc3339(day.succ_opt()?)?;
    Some((start, end))
}

/// 返回「今日」的本地日期字符串（`YYYY-MM-DD`，用户钟表上的日期）。
///
/// **这是全仓库「今日日期」的唯一入口**。此前多处各自用 `Utc::now().format(...)`
/// 算今日（ctl export 默认 --date、get_trend 日期桶等），在 UTC+8 凌晨 0-8 点
/// 会算出 UTC 的「明天」，与本地「今日」错位 → CLI 导出和 app 显示的「今日」对不上。
/// 所有需要「今天日期字符串」的地方都应调用本函数，禁止再写 `Utc::now().format("%Y-%m-%d")`。
pub fn today_local_str() -> String {
    date_offset_str(0)
}

/// 返回相对今日偏移 `days` 天的本地日期字符串（`YYYY-MM-DD`）。
/// `days=0` 即今日，`days=-1` 即昨日，`days=1` 即明日。
///
/// 与 [`today_local_str`] 同源，保证所有「本地日期」运算走同一时钟基准。
pub fn date_offset_str(days: i64) -> String {
    (Local::now() + chrono::Duration::days(days))
        .format("%Y-%m-%d")
        .to_string()
}

/// 返回「今日」区间的 `[start, end)` 边界，供 `WHERE timestamp >= start AND timestamp < end` 使用。
///
/// **时区语义**：`timestamp` 列以 UTC RFC3339 存储，但"今天"必须按**用户本地时区**
/// 定义——用户钟表上的今天，而非 UTC 日。否则本地凌晨 0 点到 UTC 换日之间的活动会被
/// 错误地归入昨天（如 UTC+8 下，本地白天 08:00 后即跨过 UTC 0 点，几乎整天数据被切到次日）。
///
/// 复用 [`local_day_range`] + [`today_local_str`]，以本地今日 `YYYY-MM-DD` 为锚。
pub fn today_range() -> (String, String) {
    let today = today_local_str();
    local_day_range(&today).unwrap_or_else(|| {
        // 极端兜底：本地日期解析理论上不会失败，退回 UTC 裸日期串（旧行为）。
        let t = Utc::now().format("%Y-%m-%d").to_string();
        let tm = (Utc::now() + chrono::Duration::days(1))
            .format("%Y-%m-%d")
            .to_string();
        (t, tm)
    })
}

// ─── 共享结构体 ───────────────────────────────────────────────────────────────

// ─── 输入计数的统一 SQL 表达式（raw 与 input_agg 两代事件形态兼容） ──────────
//
// 背景：输入采集有两种粒度（见 collector::CollectorSettings::input_granularity）——
// - raw（默认）：keyboard/press、mouse/click 各自逐事件一行，计数 = COUNT(*)；
// - minute（opt-in）：每分钟每桶一行 input_agg 计数型事件，计数藏在
//   event_data JSON（$.keys / $.clicks）里。
// 两种形态可在同一库中共存（中途切换粒度），故所有按键/点击计数查询统一走
// 这两个行级 CASE 表达式（外层 SUM），禁止再写只认 raw 形态的 COUNT(*)。
// 审查 MEDIUM：json_extract 结果一律 MAX(..., 0) 钳非负——一条负值脏数据
// （{"keys":-5}）会把"今日按键"算成负数原样展示，且使该分钟在
// keys+clicks>0 判活下被误判为不活跃。
pub(crate) const KEYS_ROW_EXPR: &str = "(CASE \
         WHEN event_type='keyboard' AND event_action='press' THEN 1 \
         WHEN event_type='keyboard' AND event_action='input_agg' AND json_valid(event_data) \
           THEN MAX(COALESCE(json_extract(event_data, '$.keys'), 0), 0) \
         ELSE 0 END)";

pub(crate) const CLICKS_ROW_EXPR: &str = "(CASE \
         WHEN event_type='mouse' AND event_action='click' THEN 1 \
         WHEN event_type='mouse' AND event_action='input_agg' AND json_valid(event_data) \
           THEN MAX(COALESCE(json_extract(event_data, '$.clicks'), 0), 0) \
         ELSE 0 END)";

/// 注入（自动化）输入的行级计数——与 KEYS/CLICKS_ROW_EXPR 同构，供 agg 缓存
/// 拆桶（db::agg 的 `input_keys_injected` / `input_clicks_injected` 桶）与
/// 异常口径扣减使用。raw 行认 hook 落库的 $.injected 布尔；input_agg 行认
/// $.injected_keys / $.injected_clicks。
pub(crate) const INJECTED_KEYS_ROW_EXPR: &str = "(CASE \
         WHEN event_type='keyboard' AND event_action='press' \
           AND COALESCE(json_extract(event_data, '$.injected'), 0) != 0 THEN 1 \
         WHEN event_type='keyboard' AND event_action='input_agg' AND json_valid(event_data) \
           THEN MAX(COALESCE(json_extract(event_data, '$.injected_keys'), 0), 0) \
         ELSE 0 END)";

pub(crate) const INJECTED_CLICKS_ROW_EXPR: &str = "(CASE \
         WHEN event_type='mouse' AND event_action='click' \
           AND COALESCE(json_extract(event_data, '$.injected'), 0) != 0 THEN 1 \
         WHEN event_type='mouse' AND event_action='input_agg' AND json_valid(event_data) \
           THEN MAX(COALESCE(json_extract(event_data, '$.injected_clicks'), 0), 0) \
         ELSE 0 END)";

/// 人侧（剔注入）行级计数 = MAX(keys - injected_keys, 0)。深夜活动 / APM 突增
/// 等异常口径统一用它——纯注入分钟（{"keys":500,"injected_keys":500}）不得被
/// 算成"深夜按键"或"行为突增"（审查修复 2026-09：自动化注入不算人）。
pub(crate) const HUMAN_KEYS_ROW_EXPR: &str = "(CASE \
         WHEN event_type='keyboard' AND event_action='press' \
           THEN (CASE WHEN COALESCE(json_extract(event_data, '$.injected'), 0) != 0 THEN 0 ELSE 1 END) \
         WHEN event_type='keyboard' AND event_action='input_agg' AND json_valid(event_data) \
           THEN MAX(MAX(COALESCE(json_extract(event_data, '$.keys'), 0), 0) - MAX(COALESCE(json_extract(event_data, '$.injected_keys'), 0), 0), 0) \
         ELSE 0 END)";

pub(crate) const HUMAN_CLICKS_ROW_EXPR: &str = "(CASE \
         WHEN event_type='mouse' AND event_action='click' \
           THEN (CASE WHEN COALESCE(json_extract(event_data, '$.injected'), 0) != 0 THEN 0 ELSE 1 END) \
         WHEN event_type='mouse' AND event_action='input_agg' AND json_valid(event_data) \
           THEN MAX(MAX(COALESCE(json_extract(event_data, '$.clicks'), 0), 0) - MAX(COALESCE(json_extract(event_data, '$.injected_clicks'), 0), 0), 0) \
         ELSE 0 END)";

/// 一分钟内的活动统计——供 [`crate::analyzer`] 专注段 / APM 序列消费。
#[derive(Debug, Clone, Default)]
pub struct MinuteStat {
    /// ISO "YYYY-MM-DDTHH:MM"
    pub minute: String,
    pub keys: i64,
    pub clicks: i64,
    pub switches: i64,
}

/// 当日 keys/clicks/active_minutes 三合一——供 [`crate::analyzer::analyze_day`] 编排层消费。
#[derive(Debug, Clone, Default)]
pub struct DayTotals {
    pub keys: i64,
    pub clicks: i64,
    pub active_minutes: i64,
}

// ─── Re-export：保持 `queries::xxx` 调用路径不变 ─────────────────────────────

pub use charts::*;
pub use history::*;
pub use maintenance::*;
pub use presence::*;
pub use summary::*;

// longest_active_streak 是纯算法（无 SQL/Connection），物理上住在 crate::time。
// 此处 re-export 保持 `queries::longest_active_streak` 调用路径稳定。
pub use crate::time::longest_active_streak;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn today_local_str_is_valid_date() {
        let s = today_local_str();
        assert!(
            chrono::NaiveDate::parse_from_str(&s, "%Y-%m-%d").is_ok(),
            "today_local_str 应返回合法 YYYY-MM-DD，实际: {s}"
        );
    }

    #[test]
    fn date_offset_str_yesterday_is_one_day_before() {
        // 两次独立读时钟在午夜瞬间会翻转为同一天（审查：midnight race），
        // 恰逢跨午夜窗口则跳过——语义已由注入锚点的测试覆盖
        let today = today_local_str();
        let yesterday = date_offset_str(-1);
        if today == yesterday {
            return;
        }
        let t = chrono::NaiveDate::parse_from_str(&today, "%Y-%m-%d").unwrap();
        let y = chrono::NaiveDate::parse_from_str(&yesterday, "%Y-%m-%d").unwrap();
        assert_eq!(
            t.pred_opt().unwrap(),
            y,
            "date_offset_str(-1) 应为今日前一天"
        );
    }

    #[test]
    fn local_day_range_start_before_end() {
        let (start, end) = local_day_range("2026-06-20").expect("valid date");
        assert!(start < end, "day range start 应早于 end: {start} < {end}");
        // start/end 应是 RFC3339（含时区偏移）
        assert!(
            start.ends_with("+00:00") || start.contains('Z') || start.contains('+'),
            "start 应为 UTC RFC3339: {start}"
        );
    }

    #[test]
    fn local_day_range_invalid_date_returns_none() {
        assert!(local_day_range("not-a-date").is_none());
        assert!(local_day_range("").is_none());
    }
}
