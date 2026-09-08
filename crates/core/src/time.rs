//! 时间解析工具
//!
//! 统一 analyzer/anomaly 中重复的 ISO 时间戳 → 分钟数转换。
//!
//! 之前 analyzer/anomaly 各自手写字符串切片解析，
//! 且用 `y*525_600 + mo*43_800 + d*1440` 估算（按 30 天/月），
//! 跨月/跨年时会算错连续段判断。本模块改用 chrono 精确计算。

use chrono::{DateTime, NaiveDateTime, Utc};

/// 把 ISO8601 时间戳（含或不含时区）转换为自纪元以来的分钟数。
///
/// 兼容 "2026-06-15T10:30:42.123+00:00" 与 "2026-06-15T10:30" 两种形式。
/// 解析失败返回 None（调用方按 0 处理，与旧行为一致）。
pub fn minutes_since_epoch(s: &str) -> Option<i64> {
    // 优先按带时区的 DateTime 解析
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Some(dt.with_timezone(&Utc).timestamp() / 60);
    }
    // 回退：按 naive（无时区）解析，视作 UTC
    let naive = NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S")
        .or_else(|_| NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M"))
        .ok()?;
    Some(naive.and_utc().timestamp() / 60)
}

/// 两时间戳相差的分钟数（end - start，向上取整，负数截为 0）。
/// 用于 analyzer::duration_min 与 anomaly 的连续段判断。
pub fn minute_diff(start: &str, end: &str) -> i64 {
    match (minutes_since_epoch(start), minutes_since_epoch(end)) {
        (Some(a), Some(b)) => (b - a).max(0),
        _ => 0,
    }
}

/// 单个时间戳的纪元分钟数（连续段判断用，解析失败返回 0）。
/// 替代 anomaly.rs 原 `parse` 闭包。
pub fn epoch_minutes_or_zero(s: &str) -> i64 {
    minutes_since_epoch(s).unwrap_or(0)
}

/// 在已知"每分钟活动"的时间序列(ISO 字符串)中，找出最长连续段及总 break 数。
///
/// 输入：已按时间升序的 "YYYY-MM-DDTHH:MM" 字符串列表
/// 输出：`(longest_min, total_breaks)`
/// - `longest_min`: 最长连续段长度(分钟数)
/// - `total_breaks`: 段间断开的次数
/// - 空输入返回 `(0, 0)`
///
/// 纯算法（无 SQL、无 Connection），供 analyzer 的碎片化指数与 anomaly 的
/// 马拉松会话检测共享。[`crate::queries`] 通过 re-export 暴露为
/// `queries::longest_active_streak` 以保持调用路径稳定。
pub fn longest_active_streak(minutes: &[String]) -> (i64, i64) {
    if minutes.is_empty() {
        return (0, 0);
    }
    let parse = |s: &str| -> i64 { epoch_minutes_or_zero(s) };

    let mut longest = 1i64;
    let mut current = 1i64;
    let mut breaks = 0i64;
    for i in 1..minutes.len() {
        let prev = parse(&minutes[i - 1]);
        let cur = parse(&minutes[i]);
        if cur - prev == 1 {
            current += 1;
            if current > longest {
                longest = current;
            }
        } else {
            breaks += 1;
            current = 1;
        }
    }
    (longest, breaks)
}
