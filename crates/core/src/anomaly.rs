//! 异常检测：深夜活动、APM 突增、马拉松会话、新应用突增
//!
//! 本模块是**纯业务规则层**——不直接执行 SQL，所有数据由 [`crate::queries`]
//! 的读取 API 预先取出后传入。这样异常规则可脱离数据库单测。
//!
//! 仅 `detect_all` / 各 `detect_*(conn, date)` 编排入口保留 `&Connection`
//! （供 Tauri command / ctl CLI 调用），内部委托纯函数。
//! 阈值常量集中在 [`crate::constants`] 模块管理。

use rusqlite::Connection;
use serde::Serialize;

use crate::constants::{
    APM_BURST_MIN_KEYS, APM_BURST_MULTIPLIER, LATE_NIGHT_HOUR_START, LATE_NIGHT_MIN_KEYS,
    MARATHON_MIN_MINUTES, NEW_APP_SURGE_MULTIPLIER,
};
use crate::queries;
use crate::Result;

#[derive(Debug, Serialize, Clone)]
pub struct Anomaly {
    pub kind: String,     // "late_night" | "apm_burst" | "marathon" | "new_app_surge"
    pub severity: String, // "info" | "warn" | "alert"
    pub message: String,
    pub detail: String,
    pub at: Option<String>, // 关联时间戳
}

// ─── 纯业务函数（无 SQL，可单测） ─────────────────────────────────────────────

/// 深夜活动：23:00 之后按键 ≥ [`LATE_NIGHT_MIN_KEYS`]。
///
/// **纯函数**：`late_night_keys` 由 [`queries::late_night_key_count`] 预先取出。
pub fn late_night_from_count(late_night_keys: i64, date: &str) -> Vec<Anomaly> {
    if late_night_keys >= LATE_NIGHT_MIN_KEYS {
        vec![Anomaly {
            kind: "late_night".into(),
            severity: "warn".into(),
            message: format!("深夜活动：{} 按键", late_night_keys),
            detail: format!(
                "在 {} {}点之后仍有 ≥{} 次按键（阈值）。建议早点休息。",
                date, LATE_NIGHT_HOUR_START, LATE_NIGHT_MIN_KEYS
            ),
            at: Some(format!("{}T{:02}:00", date, LATE_NIGHT_HOUR_START)),
        }]
    } else {
        Vec::new()
    }
}

/// APM 突增：某分钟 APM ≥ [`APM_BURST_MULTIPLIER`] × 历史均值。
///
/// **纯函数**：`burst_minutes` 由 [`queries::top_burst_minutes`] 取出，
/// `hist_avg` 由 [`queries::daily_agg_avg_apm_before`] 取出。无基线（hist_avg ≤ 0）时不报警。
pub fn apm_burst_from_data(burst_minutes: &[(String, i64)], hist_avg: f64) -> Vec<Anomaly> {
    if hist_avg <= 0.0 {
        return Vec::new(); // 没有基线，不报警
    }
    let mut out = Vec::new();
    for (minute, n) in burst_minutes {
        let ratio = *n as f64 / hist_avg;
        if ratio >= APM_BURST_MULTIPLIER {
            out.push(Anomaly {
                kind: "apm_burst".into(),
                severity: "alert".into(),
                message: format!(
                    "APM 突增：{} 达到 {:.0}（历史均值 {:.0} 的 {:.1}x）",
                    minute, *n as f64, hist_avg, ratio
                ),
                detail: "持续高强度输入，请检查是否有自动脚本或异常操作。".into(),
                at: Some(minute.clone()),
            });
        }
    }
    out
}

/// 马拉松会话：最长连续活跃段 ≥ [`MARATHON_MIN_MINUTES`] 分钟。
///
/// **纯函数**：`active_minutes` 由 [`queries::active_minutes_by_date`] 取出。
/// 复用 [`queries::longest_active_streak`] 算最长连续段。
pub fn marathon_from_minutes(active_minutes: &[String]) -> Vec<Anomaly> {
    if active_minutes.is_empty() {
        return Vec::new();
    }
    let (longest, _breaks) = queries::longest_active_streak(active_minutes);
    if longest < 1 || longest < MARATHON_MIN_MINUTES {
        return Vec::new();
    }
    // 反推 start_min/end_min：扫描 minutes 找首个等于 longest 的连续段
    let (start_min, end_min) = locate_streak(active_minutes, longest);
    if let (Some(s), Some(e)) = (start_min, end_min) {
        vec![Anomaly {
            kind: "marathon".into(),
            severity: "info".into(),
            message: format!("马拉松会话：连续活跃 {} 分钟", longest),
            detail: format!("从 {} 到 {} 持续输入无休息。", s, e),
            at: Some(s),
        }]
    } else {
        Vec::new()
    }
}

/// 在 `minutes` 中定位长度等于 `target` 的最长连续段,返回 (start, end)。
fn locate_streak(minutes: &[String], target: i64) -> (Option<String>, Option<String>) {
    let parse = |s: &str| -> i64 { crate::time::epoch_minutes_or_zero(s) };
    if minutes.is_empty() || target < 1 {
        return (None, None);
    }
    let target = target as usize;
    let mut cur_start = 0usize;
    for i in 1..=minutes.len() {
        if i == minutes.len() || parse(&minutes[i]) - parse(&minutes[i - 1]) != 1 {
            let cur_len = i - cur_start;
            if cur_len == target {
                return (
                    Some(minutes[cur_start].clone()),
                    Some(minutes[i - 1].clone()),
                );
            }
            cur_start = i;
        }
    }
    (None, None)
}

/// 新应用突增：某应用当天事件数远超历史日均（≥ [`NEW_APP_SURGE_MULTIPLIER`]×），
/// 或历史从未出现过（首次登场）。
///
/// **纯函数**：`today_apps` 由 [`queries::top_apps_by_event_types`] 取出；
/// `app_history` 回调由编排层注入 [`queries::app_history_totals`]。
/// 事件数 < 50 的应用不参与判定（噪声过滤）。
pub fn new_app_surge_from_data<F>(today_apps: &[(String, i64)], app_history: F) -> Vec<Anomaly>
where
    F: Fn(&str) -> (i64, i64),
{
    let mut out = Vec::new();
    for (app, n_today) in today_apps {
        if *n_today < 50 {
            continue; // 太小的应用不参与判定
        }
        let (hist_total, hist_days) = app_history(app);
        if hist_days == 0 {
            // 历史中从未见过，但今天事件较多 → 标记为"新应用"
            out.push(Anomaly {
                kind: "new_app_surge".into(),
                severity: "info".into(),
                message: format!("新应用首次出现：{}（{} 事件）", app, n_today),
                detail: "这是该应用首次出现在历史中".into(),
                at: None,
            });
            continue;
        }
        let hist_avg = hist_total as f64 / hist_days as f64;
        if hist_avg <= 0.0 {
            continue;
        }
        let ratio = *n_today as f64 / hist_avg;
        if ratio >= NEW_APP_SURGE_MULTIPLIER {
            out.push(Anomaly {
                kind: "new_app_surge".into(),
                severity: "warn".into(),
                message: format!(
                    "应用使用突增：{}（今天 {}，日均 {:.0}，{:.1}x）",
                    app, n_today, hist_avg, ratio
                ),
                detail: "今天使用时长是历史日均的数倍".into(),
                at: None,
            });
        }
    }
    out
}

// ─── 编排层（保留 &Connection，供 commands/ctl 调用） ────────────────────────

/// 深夜活动检测（编排入口）。
pub fn detect_late_night(conn: &Connection, date: &str) -> Result<Vec<Anomaly>> {
    let n = queries::late_night_key_count(conn, date, LATE_NIGHT_HOUR_START as i64);
    Ok(late_night_from_count(n, date))
}

/// APM 突增检测（编排入口）。
pub fn detect_apm_burst(conn: &Connection, date: &str) -> Result<Vec<Anomaly>> {
    let hist_avg = queries::daily_agg_avg_apm_before(conn, date);
    let burst = queries::top_burst_minutes(conn, date, APM_BURST_MIN_KEYS);
    Ok(apm_burst_from_data(&burst, hist_avg))
}

/// 马拉松会话检测（编排入口）。
pub fn detect_marathon_session(conn: &Connection, date: &str) -> Result<Vec<Anomaly>> {
    let minutes = queries::active_minutes_by_date(conn, date);
    Ok(marathon_from_minutes(&minutes))
}

/// 新应用突增检测（编排入口）。
pub fn detect_new_app_surge(conn: &Connection, date: &str) -> Result<Vec<Anomaly>> {
    let today = queries::top_apps_by_event_types(conn, date, 20);
    Ok(new_app_surge_from_data(&today, |app| {
        queries::app_history_totals(conn, app, date)
    }))
}

/// 整合：所有异常（编排入口）。
pub fn detect_all(conn: &Connection, date: &str) -> Result<Vec<Anomaly>> {
    let mut all = Vec::new();
    all.extend(detect_late_night(conn, date)?);
    all.extend(detect_apm_burst(conn, date)?);
    all.extend(detect_marathon_session(conn, date)?);
    all.extend(detect_new_app_surge(conn, date)?);
    Ok(all)
}
