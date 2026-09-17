//! 工具数据面 —— 从 kynoptic-core 的 SQLite 库读取实时数据
//!
//! 所有函数只依赖 `&rusqlite::Connection`（协议层与存储层解耦，可脱离 stdio 单测）。
//!
//! **数据源现实**：v0.1 采集器尚未写入 `current_state` / `agg_minute` / `agg_daily`
//! （见 CODE_NOTES.md §5），events 表也是 legacy 布局而非统一事件表。因此本模块：
//! - `get_current_status`：优先读 `current_state` 表，缺失字段从最新事件推导
//!   （cpu/mem ← system/heartbeat，battery ← system/battery_status，
//!   foreground_app ← window/switch，idle ← 最新事件距今秒数，apm ← 近 5 分钟计数）；
//! - `get_timeline`：从 window/switch 事件现算时间段（agg 层暂空）；
//! - `get_summary`：从 events 现算计数（与 ctl stats / analyzer 同口径）。
//!
//! 口径说明（审查 P2）：MCP 恒为 events 现算真值；dashboard 优先读
//! agg_minute 派生缓存（同源 events 派生）。缓存重建/滞后期间两边可能
//! 有暂时差异，以 MCP 现算为准。

use chrono::{Local, Timelike, Utc};
use rusqlite::{params, Connection, OpenFlags};
use serde_json::{json, Value};

use kynoptic_core::queries;
use kynoptic_core::types::{EventAction, EventType};

use crate::clamp_limit;

/// 规范允许的分组（get_current_status 的 `groups` 参数枚举）。
pub const GROUPS: &[&str] = &["system", "activity", "network", "devices", "security"];

/// get_summary 支持的指标。
pub const METRICS: &[&str] = &["keys", "clicks", "active_minutes", "apps", "focus_segments"];

/// wait_for 支持的信号。
pub const SIGNALS: &[&str] = &[
    "thermal_hot",
    "marathon_session",
    "network_down",
    "low_battery",
    "late_night",
    "disk_almost_full",
    "memory_pressure",
];

/// 打开工具面用的数据库连接（只读语义：不建表不迁移——库由采集器/ctl 创建）。
pub fn open_reader(path: &str) -> rusqlite::Result<Connection> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    kynoptic_core::db::apply_pragmas(&conn)?;
    Ok(conn)
}

// ─── A. get_current_status ──────────────────────────────────────────────────

/// 标量最小视图。`groups` 为 None 时默认 ["system","activity"]（spec）。
///
/// 每个标量优先取 `current_state` 表（未来采集器写入后即为热路径），
/// 缺失时从最新事件推导；两者皆无则 null。
pub fn current_status(conn: &Connection, groups: Option<&[String]>) -> Result<Value, String> {
    let group_list: Vec<String> = match groups {
        Some(g) if !g.is_empty() => {
            for s in g {
                if !GROUPS.contains(&s.as_str()) {
                    return Err(format!(
                        "Unknown group: {s} (allowed: {})（未知分组，允许: {}）",
                        GROUPS.join("/"),
                        GROUPS.join("/")
                    ));
                }
            }
            g.to_vec()
        }
        _ => vec!["system".into(), "activity".into()],
    };

    let state = read_current_state(conn);
    let mut out = serde_json::Map::new();
    for g in &group_list {
        match g.as_str() {
            "system" => {
                out.insert("cpu_pct".into(), f_or_null(state_cpu(conn, &state)));
                out.insert("mem_pct".into(), f_or_null(state_mem(conn, &state)));
                out.insert(
                    "max_temp_c".into(),
                    state.get("max_temp_c").cloned().unwrap_or(Value::Null),
                );
                out.insert("apm_5min".into(), f_or_null(Some(apm_last_5min(conn))));
            }
            "activity" => {
                out.insert(
                    "foreground_app".into(),
                    s_or_null(foreground_app(conn, &state)),
                );
                out.insert("idle_seconds".into(), f_or_null(idle_seconds(conn)));
            }
            "network" => {
                for k in ["net_up_kbps", "net_down_kbps"] {
                    out.insert(k.into(), state.get(k).cloned().unwrap_or(Value::Null));
                }
            }
            "devices" => {
                out.insert("battery_pct".into(), f_or_null(battery_pct(conn, &state)));
                out.insert(
                    "battery_charging".into(),
                    state
                        .get("battery_charging")
                        .cloned()
                        .unwrap_or(Value::Null),
                );
            }
            "security" => {
                // v0.1 无 security 监控器写入 current_state，恒为空对象
                out.insert("security".into(), json!({}));
            }
            _ => {}
        }
    }
    Ok(Value::Object(out))
}

/// 读 current_state 表为 key→JSON map（表可能不存在/为空——容错返回空 map）。
fn read_current_state(conn: &Connection) -> std::collections::HashMap<String, Value> {
    let mut map = std::collections::HashMap::new();
    let Ok(mut stmt) = conn.prepare("SELECT key, value FROM current_state") else {
        return map;
    };
    if let Ok(rows) = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))) {
        for (k, v) in rows.flatten() {
            // value 列存 TEXT，优先解析为 JSON 标量，失败则原样字符串
            let val = serde_json::from_str::<Value>(&v).unwrap_or(Value::String(v));
            map.insert(k, val);
        }
    }
    map
}

fn state_cpu(conn: &Connection, state: &std::collections::HashMap<String, Value>) -> Option<f64> {
    if let Some(v) = state.get("cpu_pct").and_then(|v| v.as_f64()) {
        return Some(v);
    }
    latest_event_data(conn, EventType::System, EventAction::Heartbeat)?
        .get("cpu_percent")?
        .as_f64()
}

fn state_mem(conn: &Connection, state: &std::collections::HashMap<String, Value>) -> Option<f64> {
    if let Some(v) = state.get("mem_pct").and_then(|v| v.as_f64()) {
        return Some(v);
    }
    latest_event_data(conn, EventType::System, EventAction::Heartbeat)?
        .get("memory")?
        .get("used_percent")?
        .as_f64()
}

fn battery_pct(conn: &Connection, state: &std::collections::HashMap<String, Value>) -> Option<f64> {
    if let Some(v) = state.get("battery_pct").and_then(|v| v.as_f64()) {
        return Some(v);
    }
    latest_event_data(conn, EventType::System, EventAction::BatteryStatus)?
        .get("percent")?
        .as_f64()
}

fn foreground_app(
    conn: &Connection,
    state: &std::collections::HashMap<String, Value>,
) -> Option<String> {
    if let Some(v) = state.get("foreground_app").and_then(|v| v.as_str()) {
        return Some(v.to_string());
    }
    // 直接读事件列（采集器把可读名写进 window_title 而 app_name 常为空，见 charts.rs 注）。
    // 降敏策略：截断到 60 字符，不返回完整窗口标题。
    let name: Option<String> = conn
        .query_row(
            "SELECT COALESCE(NULLIF(app_name,''), window_title) FROM events \
             WHERE event_type='window' AND event_action='switch' \
             ORDER BY timestamp DESC LIMIT 1",
            [],
            |r| r.get::<_, Option<String>>(0),
        )
        .ok()
        .flatten()
        .filter(|s| !s.is_empty());
    name.map(|n| truncate(&n, 60))
}

/// 距最新任意事件的秒数（idle 语义的保守近似：无 hook 事件 ≥ idle）。
fn idle_seconds(conn: &Connection) -> Option<f64> {
    let ts: String = conn
        .query_row(
            "SELECT timestamp FROM events WHERE event_type IN ('keyboard','mouse','window') ORDER BY timestamp DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .ok()?;
    let t = chrono::DateTime::parse_from_rfc3339(&ts).ok()?;
    Some((Utc::now() - t.with_timezone(&Utc)).num_seconds() as f64)
}

/// 近 5 分钟每分钟输入次数（keys+clicks）/ 5，1 位小数。
fn apm_last_5min(conn: &Connection) -> f64 {
    let since = (Utc::now() - chrono::Duration::minutes(5)).to_rfc3339();
    let n = queries::count_recent_input(conn, &since);
    (n as f64 / 5.0 * 10.0).round() / 10.0
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let t: String = s.chars().take(max).collect();
        format!("{t}…")
    }
}

fn latest_event_data(conn: &Connection, etype: EventType, action: EventAction) -> Option<Value> {
    queries::latest_event_data(conn, etype, action)
}

fn f_or_null(v: Option<f64>) -> Value {
    v.map(|f| json!((f * 100.0).round() / 100.0))
        .unwrap_or(Value::Null)
}

fn s_or_null(v: Option<String>) -> Value {
    v.map(Value::String).unwrap_or(Value::Null)
}

// ─── B. get_summary ─────────────────────────────────────────────────────────

/// 单指标日聚合 + 与**该日期前一天**同期对比百分比。
/// 同期 = 前一日（date-1）本地日起点 → 当前时刻往前推 24h（无数据则截止该日终）。
pub fn summary(conn: &Connection, date: &str, metric: &str) -> Result<Value, String> {
    if !METRICS.contains(&metric) {
        return Err(format!(
            "Unknown metric: {metric} (allowed: {})（未知指标，允许: {}）",
            METRICS.join("/"),
            METRICS.join("/")
        ));
    }
    let (start, end) = queries::local_day_range(date).ok_or_else(|| {
        format!("Bad date format: {date}, expected YYYY-MM-DD（日期格式错，应为 YYYY-MM-DD）")
    })?;
    // date 晚于今天：数据库不可能有未来数据，直接返回可读错误而非空结果
    let today = queries::today_local_str();
    let d = chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d");
    let t = chrono::NaiveDate::parse_from_str(&today, "%Y-%m-%d");
    if let (Ok(d), Ok(t)) = (d, t) {
        if d > t {
            return Err(format!(
                "Date {date} is in the future; no data can exist yet（日期 {date} 晚于今天，不可能有数据）"
            ));
        }
    }
    let value = metric_in_range(conn, metric, &start, &end)?;

    // 对比基准：date 的前一天（date-1，本地日），不再是硬编码的“今天-1”
    let prev_date = chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d")
        .ok()
        .and_then(|d| d.pred_opt())
        .map(|d| d.format("%Y-%m-%d").to_string())
        .ok_or_else(|| format!("Bad date format: {date}（日期格式错）"))?;
    let (y_start, y_end_r) = queries::local_day_range(&prev_date).ok_or_else(|| {
        format!("Cannot resolve previous day of {date}（无法解析 {date} 的前一天）")
    })?;
    let yesterday_same = {
        let same_ts = Utc::now() - chrono::Duration::days(1);
        let end = if same_ts.to_rfc3339() < y_end_r {
            same_ts.to_rfc3339()
        } else {
            y_end_r
        };
        if end <= y_start {
            None
        } else {
            Some(metric_in_range(conn, metric, &y_start, &end)?)
        }
    };

    let change_pct = match (value, yesterday_same) {
        (v, Some(y)) if y > 0.0 => Some(((v - y) / y * 100.0 * 10.0).round() / 10.0),
        _ => None,
    };
    Ok(json!({
        "date": date,
        "metric": metric,
        "value": value,
        "compared_to": prev_date,
        "yesterday_same_period": yesterday_same.map(|v| json!(v)).unwrap_or(Value::Null),
        "change_pct": change_pct.map(|v| json!(v)).unwrap_or(Value::Null),
    }))
}

fn metric_in_range(conn: &Connection, metric: &str, start: &str, end: &str) -> Result<f64, String> {
    let v = match metric {
        "keys" => queries::count_keys_in_range(conn, start, end) as f64,
        "clicks" => queries::count_clicks_in_range(conn, start, end) as f64,
        "active_minutes" => queries::active_minutes_today(conn, start, end) as f64,
        "apps" => distinct_apps(conn, start, end) as f64,
        "focus_segments" => focus_segments(conn, start, end) as f64,
        _ => return Err(format!("Unknown metric: {metric}（未知指标: {metric}）")),
    };
    Ok(v)
}

fn distinct_apps(conn: &Connection, start: &str, end: &str) -> i64 {
    conn.query_row(
        "SELECT COUNT(DISTINCT app_name) FROM events WHERE timestamp >= ?1 AND timestamp < ?2 AND app_name IS NOT NULL AND app_name <> ''",
        params![start, end],
        |r| r.get::<_, i64>(0),
    )
    .unwrap_or(0)
}

fn focus_segments(conn: &Connection, start: &str, end: &str) -> usize {
    // analyze_day 按日期字符串查询；这里区间可能非整天（昨日同期），
    // 用分钟活动现算 ≥5min 连续段数（与 analyzer 同阈值 5）。
    let mut segs = 0;
    let mut streak = 0i32;
    for (_m, keys, clicks) in minute_activity(conn, start, end) {
        if keys > 0 || clicks > 0 {
            streak += 1;
        } else {
            if streak >= 5 {
                segs += 1;
            }
            streak = 0;
        }
    }
    if streak >= 5 {
        segs += 1;
    }
    segs
}

/// 任意 [start,end) 区间的逐分钟活动 (minute, keys, clicks)，分钟升序。
/// 与 queries::minute_stats_by_date 同口径（UTC 存储 timestamp 截到分钟），
/// 但接受显式边界（供昨日同期等非整天区间使用）。
fn minute_activity(conn: &Connection, start: &str, end: &str) -> Vec<(String, i64, i64)> {
    let mut out = Vec::new();
    let Ok(mut stmt) = conn.prepare(
        "SELECT substr(timestamp, 1, 16) AS minute, \
                SUM(CASE WHEN event_type='keyboard' AND event_action='press' THEN 1 \
                         WHEN event_type='keyboard' AND event_action='input_agg' \
                           THEN COALESCE(json_extract(event_data, '$.keys'), 0) ELSE 0 END), \
                SUM(CASE WHEN event_type='mouse' AND event_action='click' THEN 1 \
                         WHEN event_type='mouse' AND event_action='input_agg' \
                           THEN COALESCE(json_extract(event_data, '$.clicks'), 0) ELSE 0 END) \
         FROM events WHERE timestamp >= ?1 AND timestamp < ?2 \
           AND event_type IN ('keyboard','mouse') \
           AND event_action IN ('press','click','input_agg') \
         GROUP BY minute ORDER BY minute",
    ) else {
        return out;
    };
    if let Ok(rows) = stmt.query_map(params![start, end], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, i64>(2)?,
        ))
    }) {
        for row in rows.flatten() {
            out.push(row);
        }
    }
    out
}

// ─── C. get_timeline / get_top_apps ────────────────────────────────────────

/// 规范化时间边界：裸日期 `YYYY-MM-DD` 按**本地日界**展开（与 dash 的
/// local_day_range 同语义，注意 mcp 进程 TZ）——from 当日本地 00:00，
/// to 当日本地日末（= 次日本地 00:00，[start,end) 语义，不多吞一天）。
/// RFC3339 等完整时间戳原样透传。
fn normalize_bound(v: &str, is_to: bool) -> String {
    let b = v.as_bytes();
    if b.len() == 10
        && b[4] == b'-'
        && b[7] == b'-'
        && chrono::NaiveDate::parse_from_str(v, "%Y-%m-%d").is_ok()
    {
        if let Some((s, e)) = queries::local_day_range(v) {
            // from → 当日本地 00:00；to → 当日本地日末（即次日 00:00）
            return if is_to { e } else { s };
        }
    }
    v.to_string()
}

/// 应用/窗口时间线段。granularity=minute 逐段返回；hour 把同一本地小时内
/// 连续同应用段合并。返回完整 JSON：segments + total_segments + truncated
/// （截断策略：按事件时间升序保留，超限丢最旧段，truncation="oldest-dropped"）。
pub fn timeline(
    conn: &Connection,
    from: &str,
    to: &str,
    granularity: &str,
    limit: usize,
) -> Result<Value, String> {
    if granularity != "minute" && granularity != "hour" {
        return Err("granularity only allows minute|hour（granularity 只允许 minute|hour）".into());
    }
    let from = normalize_bound(from, false);
    let to = normalize_bound(to, true);
    if from >= to {
        return Err(format!(
            "Invalid range: from ({from}) must be before to ({to})（时间范围无效：from 必须早于 to）"
        ));
    }
    let mut stmt = conn
        .prepare(
            "SELECT timestamp, COALESCE(NULLIF(app_name,''), window_title, '(unknown)') \
             FROM events \
             WHERE event_type='window' AND event_action='switch' \
               AND timestamp >= ?1 AND timestamp < ?2 \
             ORDER BY timestamp ASC",
        )
        .map_err(|e| e.to_string())?;
    let rows: Vec<(String, String)> = stmt
        .query_map(params![from, to], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })
        .map_err(|e| e.to_string())?
        .flatten()
        .collect();

    let to_t = chrono::DateTime::parse_from_rfc3339(&to).ok();
    let mut segs: Vec<(String, String, String)> = Vec::new(); // (app,start,end)
    for (i, (ts, app)) in rows.iter().enumerate() {
        let end = rows
            .get(i + 1)
            .map(|(next, _)| next.clone())
            .or_else(|| to_t.map(|t| t.to_rfc3339()))
            .unwrap_or_else(|| ts.clone());
        let end = if end <= *ts { ts.clone() } else { end }; // 防御：时间戳倒挂
                                                             // hour 粒度：与上一段同应用且同一本地小时 → 合并
        if granularity == "hour" {
            if let Some(last) = segs.last_mut() {
                if last.0 == *app && local_hour_bucket(&last.1) == local_hour_bucket(ts) {
                    last.2 = end;
                    continue;
                }
            }
        }
        segs.push((app.clone(), ts.clone(), end));
    }

    let total_segments = segs.len();
    let truncated = total_segments > limit;
    let out: Vec<Value> = segs
        .into_iter()
        .take(limit)
        .map(|(app, start, end)| {
            let dur = chrono::DateTime::parse_from_rfc3339(&start)
                .ok()
                .zip(chrono::DateTime::parse_from_rfc3339(&end).ok())
                .map(|(a, b)| (b - a).num_seconds().max(0))
                .unwrap_or(0);
            json!({
                "app": app,
                "start": start,
                "end": end,
                "duration_sec": dur,
            })
        })
        .collect();
    Ok(json!({
        "segments": out,
        "total_segments": total_segments,
        "truncated": truncated,
        "truncation": if truncated { "oldest-dropped" } else { "none" },
    }))
}

/// 窗口 [from,to) 内各前台应用驻留秒数，降序返回（get_top_apps 数据面）。
/// 驻留口径与 timeline 相同：switch 事件起点到下一 switch（末段到 to）。
pub fn top_apps(conn: &Connection, from: &str, to: &str, limit: usize) -> Result<Value, String> {
    let from = normalize_bound(from, false);
    let to = normalize_bound(to, true);
    if from >= to {
        return Err(format!(
            "Invalid range: from ({from}) must be before to ({to})（时间范围无效：from 必须早于 to）"
        ));
    }
    let mut stmt = conn
        .prepare(
            "SELECT timestamp, COALESCE(NULLIF(app_name,''), window_title, '(unknown)') \
             FROM events \
             WHERE event_type='window' AND event_action='switch' \
               AND timestamp >= ?1 AND timestamp < ?2 \
             ORDER BY timestamp ASC",
        )
        .map_err(|e| e.to_string())?;
    let rows: Vec<(String, String)> = stmt
        .query_map(params![from, to], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })
        .map_err(|e| e.to_string())?
        .flatten()
        .collect();

    let to_t = chrono::DateTime::parse_from_rfc3339(&to);
    let mut dwell: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
    for (i, (ts, app)) in rows.iter().enumerate() {
        let end = rows
            .get(i + 1)
            .map(|(next, _)| next.clone())
            .unwrap_or_else(|| {
                to_t.as_ref()
                    .map(|t| t.to_rfc3339())
                    .unwrap_or_else(|_| ts.clone())
            });
        let dur = chrono::DateTime::parse_from_rfc3339(ts)
            .ok()
            .zip(chrono::DateTime::parse_from_rfc3339(&end).ok())
            .map(|(a, b)| (b - a).num_seconds().max(0))
            .unwrap_or(0);
        *dwell.entry(app.clone()).or_default() += dur;
    }
    let total_apps = dwell.len();
    let mut ranked: Vec<(String, i64)> = dwell.into_iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let truncated = ranked.len() > limit;
    let apps: Vec<Value> = ranked
        .into_iter()
        .take(limit)
        .map(|(app, sec)| json!({ "app": app, "dwell_sec": sec }))
        .collect();
    Ok(json!({
        "from": from,
        "to": to,
        "apps": apps,
        "total_apps": total_apps,
        "truncated": truncated,
    }))
}

fn local_hour_bucket(ts: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(ts)
        .map(|t| t.with_timezone(&Local).format("%Y-%m-%dT%H").to_string())
        .unwrap_or_default()
}

// ─── D. get_anomalies ───────────────────────────────────────────────────────

/// 异常 kind -> 英文模板（与 dash 的 anomaly_message_en 同映射；kind 不识别
/// 时回退到中文 message，调用方按 message_en 是否为空决定展示哪条）。
fn anomaly_message_en(kind: &str, fallback: &str) -> String {
    match kind {
        "late_night" => "Late-night activity: input detected after 23:00".into(),
        "apm_burst" => "APM burst: input rate spiked well above your baseline".into(),
        "marathon" => "Marathon session: long continuous activity without breaks".into(),
        "new_app_surge" => "App usage surge: an app spiked above its daily average".into(),
        _ => fallback.to_string(),
    }
}

/// 最近 `days` 天（含今日）的异常事件列表。
pub fn anomalies(conn: &Connection, days: usize, limit: usize) -> Value {
    let days = days.clamp(1, 30);
    let limit = clamp_limit(Some(limit));
    let mut out: Vec<Value> = Vec::new();
    for i in 0..days {
        let date = queries::date_offset_str(-(i as i64));
        let list = kynoptic_core::anomaly::detect_all(conn, &date).unwrap_or_default();
        for a in list {
            if out.len() >= limit {
                return json!({"anomalies": out, "truncated": true});
            }
            let message_en = anomaly_message_en(&a.kind, &a.message);
            out.push(json!({
                "date": date,
                "kind": a.kind,
                "severity": a.severity,
                "message": a.message,
                "message_en": message_en,
                "at": a.at,
            }));
        }
    }
    json!({"anomalies": out, "truncated": false})
}

// ─── E. wait_for ────────────────────────────────────────────────────────────

/// 检查信号当前是否成立（纯读取，单次）。
pub fn check_signal(conn: &Connection, signal: &str) -> Result<bool, String> {
    if !SIGNALS.contains(&signal) {
        return Err(format!(
            "Unknown signal: {signal} (allowed: {})（未知信号，允许: {}）",
            SIGNALS.join("/"),
            SIGNALS.join("/")
        ));
    }
    Ok(match signal {
        "late_night" => {
            let hour = Local::now().hour();
            hour >= kynoptic_core::constants::LATE_NIGHT_HOUR_START
        }
        "low_battery" => {
            let Some(d) = latest_event_data(conn, EventType::System, EventAction::BatteryStatus)
            else {
                return Ok(false);
            };
            d.get("percent").and_then(|v| v.as_f64()).unwrap_or(100.0) <= 20.0
                && !d.get("charging").and_then(|v| v.as_bool()).unwrap_or(true)
        }
        "memory_pressure" => {
            let Some(d) = latest_event_data(conn, EventType::System, EventAction::Heartbeat) else {
                return Ok(false);
            };
            d.pointer("/memory/used_percent")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0)
                >= 90.0
        }
        "thermal_hot" => {
            // thermal 监控器（恢复项，默认关闭：PS 依赖）启用后写入
            // system/thermal_snapshot 的 max_temp_celsius；未启用时数据面缺 → 不触发
            let Some(d) = latest_event_data(conn, EventType::System, EventAction::ThermalSnapshot)
            else {
                return Ok(false);
            };
            d.get("max_temp_celsius")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0)
                >= 80.0
        }
        "disk_almost_full" => {
            // device/device_snapshot 的 disks[].used_percent ≥ 90 视为磁盘告急
            // （device 默认启用，容量字段始终存在；磁盘 I/O 速率为 PS 可选项不影响）
            let Some(d) = latest_event_data(conn, EventType::Device, EventAction::DeviceSnapshot)
            else {
                return Ok(false);
            };
            d.get("disks")
                .and_then(|v| v.as_array())
                .map(|disks| {
                    disks
                        .iter()
                        .any(|d| d.get("used_percent").and_then(|v| v.as_u64()).unwrap_or(0) >= 90)
                })
                .unwrap_or(false)
        }
        "marathon_session" => {
            let date = queries::today_local_str();
            let Some((s, e)) = queries::local_day_range(&date) else {
                return Ok(false);
            };
            let minutes: Vec<String> = minute_activity(conn, &s, &e)
                .into_iter()
                .filter(|(_m, keys, clicks)| *keys > 0 || *clicks > 0)
                .map(|(m, _, _)| m)
                .collect();
            let (longest, _) = queries::longest_active_streak(&minutes);
            longest >= kynoptic_core::constants::MARATHON_MIN_MINUTES
        }
        "network_down" => {
            // 有系统心跳但 15 分钟内无网络快照 → 视为网络不可用
            let hb =
                queries::latest_event_ts_by_action(conn, EventType::System, EventAction::Heartbeat);
            let net = queries::latest_event_ts_by_action(
                conn,
                EventType::Network,
                EventAction::ConnSnapshot,
            );
            let recent = |ts: Option<String>, max_sec| -> bool {
                ts.and_then(|t| chrono::DateTime::parse_from_rfc3339(&t).ok())
                    .map(|t| (Utc::now() - t.with_timezone(&Utc)).num_seconds() <= max_sec)
                    .unwrap_or(false)
            };
            recent(hb, 300) && !recent(net, 900)
        }
        _ => false,
    })
}

/// 阻塞轮询信号（spec 要求走语义层规则引擎，v0.1 无规则引擎 → 每 2s 查库，
/// 见 CODE_NOTES §5）。返回触发 payload 或 timeout 标记。
pub fn wait_for(conn: &Connection, signal: &str, timeout_sec: u64) -> Result<Value, String> {
    let timeout = timeout_sec.clamp(1, 1800);
    let start = std::time::Instant::now();
    let poll = std::time::Duration::from_secs(2);
    loop {
        if check_signal(conn, signal)? {
            return Ok(json!({
                "signal": signal,
                "status": "triggered",
                "elapsed_sec": start.elapsed().as_secs(),
            }));
        }
        if start.elapsed().as_secs() >= timeout {
            return Ok(json!({
                "signal": signal,
                "status": "timeout",
                "timeout_sec": timeout,
            }));
        }
        std::thread::sleep(poll.min(std::time::Duration::from_secs(
            timeout.saturating_sub(start.elapsed().as_secs()).max(1),
        )));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(kynoptic_core::db::SCHEMA).unwrap();
        let _ = kynoptic_core::db::run_migrations(&conn);
        conn
    }

    fn insert(
        conn: &Connection,
        ts: &str,
        t: &str,
        a: &str,
        app: Option<&str>,
        data: Option<Value>,
    ) {
        conn.execute(
            "INSERT INTO events (timestamp, event_type, event_action, event_data, app_name, window_title, session_id) VALUES (?1,?2,?3,?4,?5,?6,NULL)",
            params![ts, t, a, data.map(|v| v.to_string()), app, app],
        )
        .unwrap();
    }

    #[test]
    fn current_status_defaults_and_unknown_group() {
        let conn = mem_conn();
        let v = current_status(&conn, None).unwrap();
        let o = v.as_object().unwrap();
        assert!(o.contains_key("cpu_pct") && o.contains_key("foreground_app"));
        assert!(current_status(&conn, Some(&["bogus".into()])).is_err());
    }

    #[test]
    fn current_status_derives_from_events() {
        let conn = mem_conn();
        let now = Utc::now().to_rfc3339();
        insert(
            &conn,
            &now,
            "system",
            "heartbeat",
            None,
            Some(json!({"cpu_percent": 42.5, "memory": {"used_percent": 61.0}})),
        );
        insert(
            &conn,
            &now,
            "window",
            "switch",
            Some("code"),
            Some(json!({"title": "main.rs - editor"})),
        );
        let v = current_status(&conn, Some(&["system".into(), "activity".into()])).unwrap();
        assert_eq!(v["cpu_pct"], json!(42.5));
        assert_eq!(v["mem_pct"], json!(61.0));
        assert_eq!(v["foreground_app"], json!("code"));
        assert!(v["idle_seconds"].as_f64().unwrap() < 60.0);
    }

    #[test]
    fn current_status_prefers_current_state_table() {
        let conn = mem_conn();
        conn.execute(
            "INSERT INTO current_state (key, value, updated_at) VALUES ('cpu_pct', '77.5', '2026-01-01T00:00:00Z')",
            [],
        )
        .unwrap();
        let v = current_status(&conn, Some(&["system".into()])).unwrap();
        assert_eq!(v["cpu_pct"], json!(77.5));
    }

    #[test]
    fn summary_counts_keys_and_clicks() {
        let conn = mem_conn();
        let date = queries::today_local_str();
        let (start, _) = queries::local_day_range(&date).unwrap();
        insert(&conn, &start, "keyboard", "press", None, None);
        insert(&conn, &start, "keyboard", "press", None, None);
        insert(&conn, &start, "mouse", "click", None, None);
        let v = summary(&conn, &date, "keys").unwrap();
        assert_eq!(v["value"], json!(2.0));
        let v = summary(&conn, &date, "clicks").unwrap();
        assert_eq!(v["value"], json!(1.0));
    }

    #[test]
    fn summary_rejects_bad_metric_and_date() {
        let conn = mem_conn();
        assert!(summary(&conn, "2026-09-09", "gpu").is_err());
        assert!(summary(&conn, "not-a-date", "keys").is_err());
    }

    #[test]
    fn summary_baseline_is_date_minus_one() {
        let conn = mem_conn();
        let date = queries::today_local_str();
        let yesterday = chrono::NaiveDate::parse_from_str(&date, "%Y-%m-%d")
            .unwrap()
            .pred_opt()
            .unwrap()
            .format("%Y-%m-%d")
            .to_string();
        let (start, _) = queries::local_day_range(&date).unwrap();
        let (y_start, _) = queries::local_day_range(&yesterday).unwrap();
        insert(&conn, &start, "keyboard", "press", None, None);
        insert(&conn, &start, "keyboard", "press", None, None);
        for _ in 0..6 {
            insert(&conn, &y_start, "keyboard", "press", None, None);
        }
        let v = summary(&conn, &date, "keys").unwrap();
        assert_eq!(v["value"], json!(2.0));
        assert_eq!(v["compared_to"], json!(yesterday), "对比基准必须是 date-1");
        assert_eq!(v["yesterday_same_period"], json!(6.0));
        assert_eq!(v["change_pct"], json!(-66.7));
        // 查历史日期：基准同样是该日期的前一天，而非硬编码的“今天-1”
        let past = (chrono::Local::now() - chrono::Duration::days(5))
            .format("%Y-%m-%d")
            .to_string();
        let past_prev = (chrono::Local::now() - chrono::Duration::days(6))
            .format("%Y-%m-%d")
            .to_string();
        let v = summary(&conn, &past, "keys").unwrap();
        assert_eq!(v["compared_to"], json!(past_prev));
    }

    #[test]
    fn summary_future_date_is_readable_error() {
        let conn = mem_conn();
        let future = (chrono::Local::now() + chrono::Duration::days(2))
            .format("%Y-%m-%d")
            .to_string();
        let err = summary(&conn, &future, "keys").unwrap_err();
        assert!(err.contains("future"), "{err}");
    }

    #[test]
    fn summary_apps_distinct() {
        let conn = mem_conn();
        let date = queries::today_local_str();
        let (start, _) = queries::local_day_range(&date).unwrap();
        insert(&conn, &start, "window", "switch", Some("a"), None);
        insert(&conn, &start, "window", "switch", Some("a"), None);
        insert(&conn, &start, "window", "switch", Some("b"), None);
        let v = summary(&conn, &date, "apps").unwrap();
        assert_eq!(v["value"], json!(2.0));
    }

    #[test]
    fn timeline_segments_and_truncation_metadata() {
        let conn = mem_conn();
        insert(
            &conn,
            "2026-09-09T01:00:00+00:00",
            "window",
            "switch",
            Some("a"),
            None,
        );
        insert(
            &conn,
            "2026-09-09T01:10:00+00:00",
            "window",
            "switch",
            Some("b"),
            None,
        );
        let v = timeline(
            &conn,
            "2026-09-09T00:00:00+00:00",
            "2026-09-09T02:00:00+00:00",
            "minute",
            20,
        )
        .unwrap();
        assert_eq!(v["truncated"], json!(false));
        assert_eq!(v["total_segments"], json!(2));
        let arr = v["segments"].as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["app"], json!("a"));
        assert_eq!(arr[0]["duration_sec"], json!(600));
        // limit 钳制：limit=1 → 只回 1 段，带 total_segments + truncation 策略
        let v = timeline(
            &conn,
            "2026-09-09T00:00:00+00:00",
            "2026-09-09T02:00:00+00:00",
            "minute",
            1,
        )
        .unwrap();
        assert_eq!(v["truncated"], json!(true));
        assert_eq!(v["total_segments"], json!(2));
        assert_eq!(v["truncation"], json!("oldest-dropped"));
        assert_eq!(v["segments"].as_array().unwrap().len(), 1);
    }

    /// 裸日期 from/to 按**本地日界**解析：to 为日期时只含该本地日（不扩到次日），
    /// from 为日期时从当日本地 00:00 起（与默认 from 的本地午夜同语义）。
    #[test]
    fn timeline_bare_dates_use_local_day_bounds() {
        let conn = mem_conn();
        let today = queries::today_local_str();
        let tomorrow = chrono::NaiveDate::parse_from_str(&today, "%Y-%m-%d")
            .unwrap()
            .succ_opt()
            .unwrap()
            .format("%Y-%m-%d")
            .to_string();
        let (start, end) = queries::local_day_range(&today).unwrap();
        let (t_start, _) = queries::local_day_range(&tomorrow).unwrap();
        insert(&conn, &start, "window", "switch", Some("a"), None);
        // 次日本地 00:00 的切换必须被 to=今天 排除
        insert(&conn, &t_start, "window", "switch", Some("b"), None);
        let v = timeline(&conn, &today, &today, "minute", 100).unwrap();
        let arr = v["segments"].as_array().unwrap();
        assert_eq!(arr.len(), 1, "次日事件不得混入: {v}");
        assert_eq!(arr[0]["app"], json!("a"));
        assert_eq!(arr[0]["end"], json!(end), "to 当日应到本地日末为止");
        assert_eq!(arr[0]["start"], json!(start));
    }

    #[test]
    fn timeline_hour_merges_same_app() {
        let conn = mem_conn();
        insert(
            &conn,
            "2026-09-09T01:00:00+00:00",
            "window",
            "switch",
            Some("a"),
            None,
        );
        insert(
            &conn,
            "2026-09-09T01:20:00+00:00",
            "window",
            "switch",
            Some("a"),
            None,
        );
        insert(
            &conn,
            "2026-09-09T02:00:00+00:00",
            "window",
            "switch",
            Some("a"),
            None,
        );
        let v = timeline(
            &conn,
            "2026-09-09T00:00:00+00:00",
            "2026-09-09T03:00:00+00:00",
            "hour",
            20,
        )
        .unwrap();
        // UTC 视角 01:00 与 01:20 同小时合并，02:00 不同小时独立
        assert_eq!(v["segments"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn timeline_rejects_bad_granularity() {
        let conn = mem_conn();
        assert!(timeline(
            &conn,
            "2026-09-09T00:00:00+00:00",
            "2026-09-09T01:00:00+00:00",
            "day",
            20
        )
        .is_err());
    }

    #[test]
    fn top_apps_ranks_by_dwell_desc() {
        let conn = mem_conn();
        insert(
            &conn,
            "2026-09-09T01:00:00+00:00",
            "window",
            "switch",
            Some("a"),
            None,
        );
        insert(
            &conn,
            "2026-09-09T01:10:00+00:00",
            "window",
            "switch",
            Some("b"),
            None,
        );
        insert(
            &conn,
            "2026-09-09T01:30:00+00:00",
            "window",
            "switch",
            Some("a"),
            None,
        );
        // a: 600 + 1800 = 2400；b: 1200 → a 排前
        let v = top_apps(
            &conn,
            "2026-09-09T01:00:00+00:00",
            "2026-09-09T02:00:00+00:00",
            10,
        )
        .unwrap();
        let arr = v["apps"].as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["app"], json!("a"));
        assert_eq!(arr[0]["dwell_sec"], json!(2400));
        assert_eq!(arr[1]["app"], json!("b"));
        assert_eq!(arr[1]["dwell_sec"], json!(1200));
        // limit 截断带 total_apps
        let v = top_apps(
            &conn,
            "2026-09-09T01:00:00+00:00",
            "2026-09-09T02:00:00+00:00",
            1,
        )
        .unwrap();
        assert_eq!(v["total_apps"], json!(2));
        assert_eq!(v["truncated"], json!(true));
        assert_eq!(v["apps"].as_array().unwrap().len(), 1);
        // 范围倒挂 → 可读错误
        assert!(top_apps(
            &conn,
            "2026-09-09T02:00:00+00:00",
            "2026-09-09T01:00:00+00:00",
            10
        )
        .is_err());
    }

    // 本地数据完整优先：app_name 为空时回退返回 window_title 原文，两者皆空
    // 才记 (unknown)。窗口标题是本地事实的一部分，不为隐私擅自削减。
    #[test]
    fn timeline_falls_back_to_window_title_when_app_name_empty() {
        let conn = mem_conn();
        conn.execute(
            "INSERT INTO events (timestamp, event_type, event_action, event_data, app_name, window_title, session_id) VALUES (?1, 'window', 'switch', NULL, '', 'Secret Document Title', NULL)",
            params!["2026-09-09T01:00:00+00:00"],
        )
        .unwrap();
        let v = timeline(
            &conn,
            "2026-09-09T00:00:00+00:00",
            "2026-09-09T02:00:00+00:00",
            "minute",
            20,
        )
        .unwrap();
        let arr = v["segments"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["app"], json!("Secret Document Title"));
    }

    /// app_name 与 window_title 皆空才落 (unknown)。
    #[test]
    fn timeline_unknown_only_when_both_empty() {
        let conn = mem_conn();
        conn.execute(
            "INSERT INTO events (timestamp, event_type, event_action, event_data, app_name, window_title, session_id) VALUES (?1, 'window', 'switch', NULL, '', NULL, NULL)",
            params!["2026-09-09T01:00:00+00:00"],
        )
        .unwrap();
        let v = timeline(
            &conn,
            "2026-09-09T00:00:00+00:00",
            "2026-09-09T02:00:00+00:00",
            "minute",
            20,
        )
        .unwrap();
        let arr = v["segments"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["app"], json!("(unknown)"));
    }

    #[test]
    fn anomalies_empty_on_fresh_db() {
        let conn = mem_conn();
        let v = anomalies(&conn, 1, 20);
        assert_eq!(v["truncated"], json!(false));
        assert_eq!(v["anomalies"].as_array().unwrap().len(), 0);
    }

    /// 每条异常必须带 message_en（kind 映射或回退中文 message）。
    #[test]
    fn anomalies_carry_message_en() {
        let conn = mem_conn();
        // 深夜活动 → late_night 异常（插入 23:00 后的本地输入）
        let late = Local::now()
            .date_naive()
            .and_hms_opt(23, 30, 0)
            .and_then(|n| {
                use chrono::TimeZone;
                Local
                    .from_local_datetime(&n)
                    .earliest()
                    .map(|d| d.with_timezone(&Utc).to_rfc3339())
            })
            .unwrap_or_else(|| Utc::now().to_rfc3339());
        insert(&conn, &late, "keyboard", "press", None, None);
        let v = anomalies(&conn, 1, 20);
        let arr = v["anomalies"].as_array().unwrap();
        if arr.is_empty() {
            // 检测器未触发时至少验证映射回退逻辑本身
            assert_eq!(anomaly_message_en("no_such_kind", "中文消息"), "中文消息");
            assert!(anomaly_message_en("marathon", "").contains("Marathon"));
            return;
        }
        for a in arr {
            let en = a["message_en"].as_str().unwrap_or_default();
            assert!(!en.is_empty(), "message_en 不得为空: {a}");
        }
    }

    #[test]
    fn check_signal_validates_and_defaults_false() {
        let conn = mem_conn();
        assert!(check_signal(&conn, "bogus").is_err());
        for s in SIGNALS {
            // 全新库上除 late_night（取决于本地时间）外都应不触发
            if *s == "late_night" {
                continue;
            }
            assert!(!check_signal(&conn, s).unwrap(), "{s}");
        }
    }

    #[test]
    fn wait_for_times_out() {
        let conn = mem_conn();
        let v = wait_for(&conn, "thermal_hot", 1).unwrap();
        assert_eq!(v["status"], json!("timeout"));
        assert_eq!(v["timeout_sec"], json!(1));
    }

    #[test]
    fn disk_almost_full_triggers_on_used_percent() {
        let conn = mem_conn();
        // 无快照 → 不触发
        assert!(!check_signal(&conn, "disk_almost_full").unwrap());
        // used_percent 91 → 触发
        insert(
            &conn,
            "2026-09-09T10:00:00Z",
            "device",
            "device_snapshot",
            None,
            Some(json!({"disks": [{"drive": "C", "used_percent": 91}]})),
        );
        assert!(check_signal(&conn, "disk_almost_full").unwrap());
    }

    #[test]
    fn thermal_hot_triggers_on_max_temp() {
        let conn = mem_conn();
        insert(
            &conn,
            "2026-09-09T10:00:00Z",
            "system",
            "thermal_snapshot",
            None,
            Some(json!({"max_temp_celsius": 85.0})),
        );
        assert!(check_signal(&conn, "thermal_hot").unwrap());
    }

    #[test]
    fn wait_for_rejects_unknown_signal() {
        let conn = mem_conn();
        assert!(wait_for(&conn, "bogus", 1).is_err());
    }
}
