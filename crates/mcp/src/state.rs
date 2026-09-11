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
                    return Err(format!("未知分组: {s}（允许: {}）", GROUPS.join("/")));
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

/// 单指标日聚合 + 与昨日**同期**对比百分比。
/// 同期 = 昨日本地日起点 → 当前时刻往前推 24h（无数据则截止昨日日终）。
pub fn summary(conn: &Connection, date: &str, metric: &str) -> Result<Value, String> {
    if !METRICS.contains(&metric) {
        return Err(format!("未知指标: {metric}（允许: {}）", METRICS.join("/")));
    }
    let (start, end) = queries::local_day_range(date)
        .ok_or_else(|| format!("日期格式错: {date}（应为 YYYY-MM-DD）"))?;
    let value = metric_in_range(conn, metric, &start, &end)?;

    // 昨日同期
    let y_date = queries::date_offset_str(-1);
    let yesterday_same = local_date(&y_date).and_then(|y_start| {
        let same_ts = Utc::now() - chrono::Duration::days(1);
        let y_end_r = queries::local_day_range(&y_date).map(|(_, e)| e)?;
        let end = if same_ts.to_rfc3339() < y_end_r {
            same_ts.to_rfc3339()
        } else {
            y_end_r
        };
        if end <= y_start {
            None
        } else {
            Some(metric_in_range(conn, metric, &y_start, &end).ok()?)
        }
    });

    let change_pct = match (value, yesterday_same) {
        (v, Some(y)) if y > 0.0 => Some(((v - y) / y * 100.0 * 10.0).round() / 10.0),
        _ => None,
    };
    Ok(json!({
        "date": date,
        "metric": metric,
        "value": value,
        "yesterday_same_period": yesterday_same.map(|v| json!(v)).unwrap_or(Value::Null),
        "change_pct": change_pct.map(|v| json!(v)).unwrap_or(Value::Null),
    }))
}

/// 本地日期字符串 → 本地午夜的 UTC 时刻。
fn local_date(date: &str) -> Option<String> {
    queries::local_day_range(date).map(|(s, _)| s)
}

fn metric_in_range(conn: &Connection, metric: &str, start: &str, end: &str) -> Result<f64, String> {
    let v = match metric {
        "keys" => queries::count_keys_in_range(conn, start, end) as f64,
        "clicks" => queries::count_clicks_in_range(conn, start, end) as f64,
        "active_minutes" => queries::active_minutes_today(conn, start, end) as f64,
        "apps" => distinct_apps(conn, start, end) as f64,
        "focus_segments" => focus_segments(conn, start, end) as f64,
        _ => return Err(format!("未知指标: {metric}")),
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

// ─── C. get_timeline ────────────────────────────────────────────────────────

/// 应用/窗口时间线段。granularity=minute 逐段返回；hour 把同一本地小时内
/// 连续同应用段合并。返回 (segments, truncated)。
pub fn timeline(
    conn: &Connection,
    from: &str,
    to: &str,
    granularity: &str,
    limit: usize,
) -> Result<(Value, bool), String> {
    if granularity != "minute" && granularity != "hour" {
        return Err("granularity 只允许 minute|hour".into());
    }
    let limit = clamp_limit(Some(limit));
    let mut stmt = conn
        .prepare(
            "SELECT timestamp, COALESCE(NULLIF(app_name,''), window_title, '(unknown)') \
             FROM events \
             WHERE event_type='window' AND event_action='switch' \
               AND timestamp >= ?1 AND timestamp < ?2 \
             ORDER BY timestamp ASC LIMIT ?3",
        )
        .map_err(|e| e.to_string())?;
    let mut rows: Vec<(String, String)> = Vec::new();
    let mut truncated = false;
    if let Ok(mapped) = stmt.query_map(params![from, to, limit as i64 + 1], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    }) {
        for r in mapped.flatten() {
            if rows.len() >= limit {
                truncated = true;
                break;
            }
            rows.push(r);
        }
    }

    let to_t = chrono::DateTime::parse_from_rfc3339(to).ok();
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

    let out: Vec<Value> = segs
        .into_iter()
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
    Ok((Value::Array(out), truncated))
}

fn local_hour_bucket(ts: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(ts)
        .map(|t| t.with_timezone(&Local).format("%Y-%m-%dT%H").to_string())
        .unwrap_or_default()
}

// ─── D. get_anomalies ───────────────────────────────────────────────────────

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
            out.push(json!({
                "date": date,
                "kind": a.kind,
                "severity": a.severity,
                "message": a.message,
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
        return Err(format!("未知信号: {signal}（允许: {}）", SIGNALS.join("/")));
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
    fn timeline_segments_and_clamp() {
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
        let (v, trunc) = timeline(
            &conn,
            "2026-09-09T00:00:00+00:00",
            "2026-09-09T02:00:00+00:00",
            "minute",
            20,
        )
        .unwrap();
        assert!(!trunc);
        let arr = v.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["app"], json!("a"));
        assert_eq!(arr[0]["duration_sec"], json!(600));
        // limit 钳制：limit=1 → 只回 1 段且 truncated
        let (v, trunc) = timeline(
            &conn,
            "2026-09-09T00:00:00+00:00",
            "2026-09-09T02:00:00+00:00",
            "minute",
            1,
        )
        .unwrap();
        assert!(trunc);
        assert_eq!(v.as_array().unwrap().len(), 1);
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
        let (v, _) = timeline(
            &conn,
            "2026-09-09T00:00:00+00:00",
            "2026-09-09T03:00:00+00:00",
            "hour",
            20,
        )
        .unwrap();
        // UTC 视角 01:00 与 01:20 同小时合并，02:00 不同小时独立
        assert_eq!(v.as_array().unwrap().len(), 2);
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
    fn anomalies_empty_on_fresh_db() {
        let conn = mem_conn();
        let v = anomalies(&conn, 1, 20);
        assert_eq!(v["truncated"], json!(false));
        assert_eq!(v["anomalies"].as_array().unwrap().len(), 0);
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
