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
mod summary;

// ─── 共享 helper（子模块通过 super:: 引用） ──────────────────────────────────

/// 返回本地时区相对 UTC 的偏移秒数（东半球为正，如 UTC+8 返回 28800）。
pub(crate) fn local_offset_seconds() -> i64 {
    Local::now().offset().local_minus_utc() as i64
}

/// 返回 SQLite `datetime()` 用的本地偏移修饰符字符串，如 `"+28800 seconds"` / `"-18000 seconds"`。
///
/// 供按小时/分钟/日期聚合的 SQL：`datetime(timestamp, ?modifier)` 先把 UTC 存储的
/// timestamp 换成本地时刻，再 `substr` 取桶——否则"小时分布"图会整体错位（如 UTC+8 下
/// 本地下午 14:00 的活动被画到 UTC 06:00 那一柱）。
pub(crate) fn local_offset_modifier() -> String {
    let secs = local_offset_seconds();
    format!(
        "{}{} seconds",
        if secs >= 0 { "+" } else { "-" },
        secs.abs()
    )
}

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

pub fn get_count_i64(row: &Row<'_>) -> rusqlite::Result<i64> {
    row.get(0)
}

pub fn get_string(row: &Row<'_>) -> rusqlite::Result<String> {
    row.get(0)
}

/// 将「本地某一天」`date`（`YYYY-MM-DD`，用户钟表上的日期）转成
/// `[start, end)` 的 UTC RFC3339 边界，供 `WHERE timestamp >= start AND timestamp < end`。
///
/// 与 [`today_range`] 同源：所有"按天"查询都应走这里，确保**实时面板的今日**与
/// **分析面板的某天**对"天"的定义一致（均为用户本地时区的自然日），而非 UTC 日。
///
/// `date` 解析失败时返回 `None`，调用方应回退到原始字符串前缀匹配或视为空。
pub fn local_day_range(date: &str) -> Option<(String, String)> {
    use chrono::{NaiveDate, TimeZone};
    let day = NaiveDate::parse_from_str(date, "%Y-%m-%d").ok()?;
    let to_utc_rfc3339 = |d: NaiveDate| -> Option<String> {
        let naive = d.and_hms_opt(0, 0, 0)?;
        Local
            .from_local_datetime(&naive)
            .earliest()
            .map(|dt| dt.with_timezone(&Utc).to_rfc3339())
            .or_else(|| Some(Utc.from_utc_datetime(&naive).to_rfc3339()))
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
        let today = today_local_str();
        let yesterday = date_offset_str(-1);
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
