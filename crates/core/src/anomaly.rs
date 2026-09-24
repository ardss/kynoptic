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
    APM_BURST_MIN_KEYS, APM_BURST_MULTIPLIER, DEFAULT_PRESENCE_BRIDGE_MINUTES, LATE_NIGHT_END_HOUR,
    LATE_NIGHT_HOUR_START, LATE_NIGHT_MIN_KEYS, MARATHON_MIN_MINUTES, NEW_APP_SURGE_MULTIPLIER,
};
use crate::queries;
use crate::Result;

#[derive(Debug, Serialize, Clone)]
pub struct Anomaly {
    pub kind: String,       // "late_night" | "apm_burst" | "marathon" | "new_app_surge"
    pub kind_label: String, // 中文友好标签，供仪表盘直接展示（"深夜活动"等）
    pub severity: String,   // "info" | "warn" | "alert"
    pub message: String,
    pub detail: String,
    pub at: Option<String>, // 关联时间戳
}

// ─── 纯业务函数（无 SQL，可单测） ─────────────────────────────────────────────

/// 深夜活动：[23:00, 次日 06:00) 窗口内按键 ≥ [`LATE_NIGHT_MIN_KEYS`]。
///
/// 口径（统一 2026-09）：深夜窗口 = 本地 23:00-06:00（跨午夜，含 0-6 点），
/// 与 insights 深夜卡为同一口径（[`constants::LATE_NIGHT_HOUR_START`] /
/// [`constants::LATE_NIGHT_END_HOUR`]）。
///
/// **纯函数**：`late_night_keys` 由 [`queries::late_night_key_count`] 预先取出。
pub fn late_night_from_count(late_night_keys: i64, date: &str) -> Vec<Anomaly> {
    if late_night_keys >= LATE_NIGHT_MIN_KEYS {
        vec![Anomaly {
            kind: "late_night".into(),
            kind_label: "深夜活动".into(),
            severity: "warn".into(),
            message: format!("深夜活动：{} 按键", late_night_keys),
            detail: format!(
                "在 {} {}点之后仍有 ≥{} 次按键（阈值）。建议早点休息。",
                date, LATE_NIGHT_HOUR_START, LATE_NIGHT_MIN_KEYS
            ),
            // Wave20 P1：窗口含 0-6 点（属 date 前夜的跨午夜段），标成
            // dateT23:00 会得到"尚未发生"的未来时刻——降为日期粒度
            at: None,
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
                kind_label: "APM 突增".into(),
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

/// 马拉松会话：最长连续**人在场**段 ≥ [`MARATHON_MIN_MINUTES`] 分钟。
///
/// **口径（统一 2026-09，审查修复）**：数据源是
/// [`queries::human_minutes_by_date`] 的 human 分钟序列（剔注入输入、含滚轮，
/// 与总览「今日在场」/ presence 完全同源）——此前误用 `active_minutes_by_date`
/// 的 input_keys/clicks 桶（含自动化注入输入），同一分钟在异常页算"在场"、
/// 在总览算"自动化"，违背「自动化脚本敲得再响也不算人在场」的承诺。
///
/// - human 分钟先按 `bridge_minutes` 桥接补洞再求最长连续段——相邻间隙
///   ≤ bridge 分钟（无输入阅读）按连续计。`bridge_minutes` 读 settings 的
///   `presence_bridge_minutes`（0-15，默认 2，与 presence/专注块同一口径源）。
///
/// **纯函数**：`human_minutes` 为 UTC 纪元分钟序列
/// （由 [`queries::human_minutes_by_date`] 取出）。
pub fn marathon_from_epoch_minutes(human_minutes: &[i64], bridge_minutes: u32) -> Vec<Anomaly> {
    if human_minutes.is_empty() {
        return Vec::new();
    }
    let bridge = i64::from(bridge_minutes.min(15));
    // 输入为 UTC 纪元分钟（升序去重由取数方保证，这里再防御一次），
    // 按 bridge 补洞后求最长连续段。
    let mut mins: Vec<i64> = human_minutes.to_vec();
    mins.sort_unstable();
    mins.dedup();
    let mut bridged: Vec<i64> = Vec::with_capacity(mins.len());
    bridged.push(mins[0]);
    for &m in &mins[1..] {
        let prev = bridged[bridged.len() - 1];
        // 间隙 (1, bridge+1] 分钟：中间缺失的 ≤ bridge 分钟按"无输入阅读"补齐
        if m - prev > 1 && m - prev <= bridge + 1 {
            bridged.extend(prev + 1..m);
        }
        bridged.push(m);
    }
    let longest = longest_consecutive_len(&bridged);
    if longest < MARATHON_MIN_MINUTES {
        return Vec::new();
    }
    // 反推 start_min/end_min：扫描 bridged 找首个等于 longest 的连续段
    if let Some((s, e)) = locate_streak_minutes(&bridged, longest) {
        // 纪元分钟 -> ISO "YYYY-MM-DDTHH:MM"（**本地时区**，交叉审查 P2：
        // 此前用 UTC 格式化，本地 12:00 的马拉松显示为 UTC 04:00；与 dash
        // 其他显示统一。解析异常时回退空串，不 panic）
        let fmt = |m: i64| -> String {
            chrono::DateTime::from_timestamp(m * 60, 0)
                .map(|t| {
                    t.with_timezone(&chrono::Local)
                        .format("%Y-%m-%dT%H:%M")
                        .to_string()
                })
                .unwrap_or_default()
        };
        vec![Anomaly {
            kind: "marathon".into(),
            kind_label: "长时间连续在场".into(),
            severity: "info".into(),
            message: format!("连续在场 {} 分钟（长时间无离开）", longest),
            detail: format!("从 {} 到 {} 持续输入无休息。", fmt(s), fmt(e)),
            at: Some(fmt(s)),
        }]
    } else {
        Vec::new()
    }
}

/// [`marathon_from_epoch_minutes`] 的本地钟面串入口：把 "YYYY-MM-DDTHH:MM"
/// 本地钟面串按**本地时区**折算成纪元分钟再判定（保留给以钟面串为数据源的
/// 既有调用/测试；新代码请直接用纪元分钟入口）。
pub fn marathon_from_minutes(active_minutes: &[String], bridge_minutes: u32) -> Vec<Anomaly> {
    if active_minutes.is_empty() {
        return Vec::new();
    }
    let mut mins: Vec<i64> = active_minutes
        .iter()
        .map(|s| crate::time::local_epoch_minutes_or_zero(s))
        .collect();
    mins.sort_unstable();
    mins.dedup();
    marathon_from_epoch_minutes(&mins, bridge_minutes)
}

/// 排序 i64 分钟序列的最长连续段长度（相邻差 1 计连续）。
fn longest_consecutive_len(minutes: &[i64]) -> i64 {
    if minutes.is_empty() {
        return 0;
    }
    let mut longest = 1i64;
    let mut current = 1i64;
    for w in minutes.windows(2) {
        if w[1] - w[0] == 1 {
            current += 1;
            if current > longest {
                longest = current;
            }
        } else {
            current = 1;
        }
    }
    longest
}

/// 在排序 `minutes` 中定位长度等于 `target` 的首个连续段，返回纪元分钟 (start, end)。
fn locate_streak_minutes(minutes: &[i64], target: i64) -> Option<(i64, i64)> {
    if minutes.is_empty() || target < 1 {
        return None;
    }
    let target = target as usize;
    let mut cur_start = 0usize;
    for i in 1..=minutes.len() {
        if i == minutes.len() || minutes[i] - minutes[i - 1] != 1 {
            if i - cur_start == target {
                return Some((minutes[cur_start], minutes[i - 1]));
            }
            cur_start = i;
        }
    }
    None
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
                kind_label: "新应用激增".into(),
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
                kind_label: "新应用激增".into(),
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
    let n = queries::late_night_key_count(
        conn,
        date,
        LATE_NIGHT_HOUR_START as i64,
        LATE_NIGHT_END_HOUR as i64,
    );
    Ok(late_night_from_count(n, date))
}

/// APM 突增检测（编排入口）。
pub fn detect_apm_burst(conn: &Connection, date: &str) -> Result<Vec<Anomaly>> {
    let hist_avg = queries::daily_agg_avg_apm_before(conn, date);
    let burst = queries::top_burst_minutes(conn, date, APM_BURST_MIN_KEYS);
    Ok(apm_burst_from_data(&burst, hist_avg))
}

/// 马拉松会话检测（编排入口）。数据源为 [`queries::human_minutes_by_date`]
/// 的人在场的纪元分钟序列（剔注入、含滚轮，与总览「今日在场」同源）；
/// `bridge_minutes` 见 [`marathon_from_epoch_minutes`]。
pub fn detect_marathon_session(
    conn: &Connection,
    date: &str,
    bridge_minutes: u32,
) -> Result<Vec<Anomaly>> {
    let minutes = queries::human_minutes_by_date(conn, date);
    Ok(marathon_from_epoch_minutes(&minutes, bridge_minutes))
}

/// 多日马拉松检测：[`queries::human_minutes_by_days`] 整窗一次取数后逐日跑
/// 同一检测算法（长跨度 30 天此前按日重扫 events，成本随天数严格线性）。
/// 返回与 `dates` 一一对应的检测结果。
pub fn detect_marathon_days(
    conn: &Connection,
    dates: &[String],
    bridge_minutes: u32,
) -> Result<Vec<Vec<Anomaly>>> {
    Ok(queries::human_minutes_by_days(conn, dates)
        .into_iter()
        .map(|(_, minutes)| marathon_from_epoch_minutes(&minutes, bridge_minutes))
        .collect())
}

/// 新应用突增检测（编排入口）。
pub fn detect_new_app_surge(conn: &Connection, date: &str) -> Result<Vec<Anomaly>> {
    let today = queries::top_apps_by_event_types(conn, date, 20);
    Ok(new_app_surge_from_data(&today, |app| {
        queries::app_history_totals(conn, app, date)
    }))
}

/// 整合：所有异常（编排入口）。marathon 用默认桥接阈值
/// [`constants::DEFAULT_PRESENCE_BRIDGE_MINUTES`]；dashboard 入口应改用
/// [`detect_all_with_bridge`] 注入 settings 的 `presence_bridge_minutes`。
pub fn detect_all(conn: &Connection, date: &str) -> Result<Vec<Anomaly>> {
    detect_all_with_bridge(conn, date, DEFAULT_PRESENCE_BRIDGE_MINUTES)
}

/// [`detect_all`] 的桥接注入版：`bridge_minutes` 来自 settings 的
/// `presence_bridge_minutes`（全站连续性口径统一源）。
pub fn detect_all_with_bridge(
    conn: &Connection,
    date: &str,
    bridge_minutes: u32,
) -> Result<Vec<Anomaly>> {
    detect_all_days_with_bridge(
        conn,
        std::slice::from_ref(&date.to_string()),
        bridge_minutes,
    )
    .map(|v| {
        v.into_iter()
            .next()
            .map(|(_, list)| list)
            .unwrap_or_default()
    })
}

/// 多日整合版 [`detect_all_with_bridge`]（长跨度查询路径）：marathon 的
/// 人在场分钟改为整窗一次取数（[`detect_marathon_days`]），其余三类检测器
/// 本就消费 agg_minute 缓存、保持逐日调用不变。返回 (日期, 异常列表) 与
/// `dates` 一一对应，逐日结果与逐日调用 [`detect_all_with_bridge`] 等价。
pub fn detect_all_days_with_bridge(
    conn: &Connection,
    dates: &[String],
    bridge_minutes: u32,
) -> Result<Vec<(String, Vec<Anomaly>)>> {
    let marathons = detect_marathon_days(conn, dates, bridge_minutes)?;
    dates
        .iter()
        .zip(marathons)
        .map(|(date, marathon)| {
            let mut all = Vec::new();
            all.extend(detect_late_night(conn, date)?);
            all.extend(detect_apm_burst(conn, date)?);
            all.extend(marathon);
            all.extend(detect_new_app_surge(conn, date)?);
            Ok((date.clone(), all))
        })
        .collect()
}
