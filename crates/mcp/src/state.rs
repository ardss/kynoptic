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
use crate::server::trunc_echo;

/// 错误回显里入参原文的最大字符数（防带宽放大：超长入参不再整段回显）。
const ECHO_MAX_CHARS: usize = 64;

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
/// 用只读 PRAGMA 子集：apply_pragmas 的 journal_mode=WAL 是写操作，
/// 只读连接上会失败（见 schema.rs 注）。
pub fn open_reader(path: &str) -> rusqlite::Result<Connection> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    kynoptic_core::db::apply_pragmas_readonly(&conn)?;
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
                        "Unknown group: {} (allowed: {})（未知分组，允许: {}）",
                        trunc_echo(s, ECHO_MAX_CHARS),
                        GROUPS.join("/"),
                        GROUPS.join("/")
                    ));
                }
            }
            g.to_vec()
        }
        _ => vec!["system".into(), "activity".into()],
    };

    let state = read_current_state(conn)?;
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

/// 读 current_state 表为 key→JSON map。
/// 审查 P1：表不存在（v0.1 采集器未建/未写）属正常空态；但 prepare/query
/// 的真实 SQL 错误不得吞成空 map（会伪装成"无状态数据"）——向上传播为工具错误。
fn read_current_state(
    conn: &Connection,
) -> Result<std::collections::HashMap<String, Value>, String> {
    let mut map = std::collections::HashMap::new();
    let mut stmt = conn
        .prepare("SELECT key, value FROM current_state")
        .map_err(|e| format!("Failed to read current_state: {e}（读取 current_state 失败）"))?;
    let rows = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        .map_err(|e| format!("Failed to read current_state: {e}（读取 current_state 失败）"))?;
    for row in rows {
        let (k, v) = row
            .map_err(|e| format!("Failed to read current_state: {e}（读取 current_state 失败）"))?;
        // value 列存 TEXT，优先解析为 JSON 标量，失败则原样字符串
        let val = serde_json::from_str::<Value>(&v).unwrap_or(Value::String(v));
        map.insert(k, val);
    }
    Ok(map)
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
    // 审查 P2：时钟回拨/未来时间戳时负数 idle 会误导 LLM 消费者，钳到 0。
    Some(((Utc::now() - t.with_timezone(&Utc)).num_seconds().max(0)) as f64)
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
            "Unknown metric: {} (allowed: {})（未知指标，允许: {}）",
            trunc_echo(metric, ECHO_MAX_CHARS),
            METRICS.join("/"),
            METRICS.join("/")
        ));
    }
    let (start, end) = queries::local_day_range(date).ok_or_else(|| {
        format!(
            "Bad date format: {}, expected YYYY-MM-DD（日期格式错，应为 YYYY-MM-DD）",
            trunc_echo(date, ECHO_MAX_CHARS)
        )
    })?;
    // date 晚于今天：数据库不可能有未来数据，直接返回可读错误而非空结果
    let today = queries::today_local_str();
    let d = chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d");
    let t = chrono::NaiveDate::parse_from_str(&today, "%Y-%m-%d");
    if let (Ok(d), Ok(t)) = (d, t) {
        if d > t {
            return Err(format!(
                "Date {} is in the future; no data can exist yet（日期 {} 晚于今天，不可能有数据）",
                trunc_echo(date, ECHO_MAX_CHARS),
                trunc_echo(date, ECHO_MAX_CHARS)
            ));
        }
    }
    let value = metric_in_range(conn, metric, &start, &end)?;

    // 对比基准：date 的前一天（date-1，本地日），不再是硬编码的“今天-1”
    let prev_date = chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d")
        .ok()
        .and_then(|d| d.pred_opt())
        .map(|d| d.format("%Y-%m-%d").to_string())
        .ok_or_else(|| {
            format!(
                "Bad date format: {}（日期格式错）",
                trunc_echo(date, ECHO_MAX_CHARS)
            )
        })?;
    let (y_start, y_end_r) = queries::local_day_range(&prev_date).ok_or_else(|| {
        format!(
            "Cannot resolve previous day of {}（无法解析 {} 的前一天）",
            trunc_echo(date, ECHO_MAX_CHARS),
            trunc_echo(date, ECHO_MAX_CHARS)
        )
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
        "apps" => distinct_apps(conn, start, end)? as f64,
        "focus_segments" => focus_segments(conn, start, end)? as f64,
        _ => return Err(format!("Unknown metric: {metric}（未知指标: {metric}）")),
    };
    Ok(v)
}

/// 审查 P1：SQL 错误不得吞成 0（伪装成"无应用使用"）——prepare/查询错误
/// 向上传播；仅 COUNT 空结果集（QueryReturnedNoRows）按 0 处理。
fn distinct_apps(conn: &Connection, start: &str, end: &str) -> Result<i64, String> {
    match conn.query_row(
        "SELECT COUNT(DISTINCT app_name) FROM events WHERE timestamp >= ?1 AND timestamp < ?2 AND app_name IS NOT NULL AND app_name <> ''",
        params![start, end],
        |r| r.get::<_, i64>(0),
    ) {
        Ok(n) => Ok(n),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(0),
        Err(e) => Err(format!("Failed to count distinct apps: {e}（统计应用数失败）")),
    }
}

fn focus_segments(conn: &Connection, start: &str, end: &str) -> Result<usize, String> {
    // analyze_day 按日期字符串查询；这里区间可能非整天（昨日同期），
    // 用分钟活动现算 ≥5min 连续段数（与 analyzer 同阈值 5）。
    let mut segs = 0;
    let mut streak = 0i32;
    for (_m, keys, clicks) in minute_activity(conn, start, end)? {
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
    Ok(segs)
}

/// 任意 [start,end) 区间的逐分钟活动 (minute, keys, clicks)，分钟升序。
/// 与 queries::minute_stats_by_date 同口径（UTC 存储 timestamp 截到分钟），
/// 但接受显式边界（供昨日同期等非整天区间使用）。
/// 审查 P1：prepare/query_map 失败不得静默返回空（伪装成"无活动"），向上传播。
fn minute_activity(
    conn: &Connection,
    start: &str,
    end: &str,
) -> Result<Vec<(String, i64, i64)>, String> {
    let mut out = Vec::new();
    let mut stmt = conn
        .prepare(
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
        )
        .map_err(|e| format!("Failed to query minute activity: {e}（查询分钟活动失败）"))?;
    let rows = stmt
        .query_map(params![start, end], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
            ))
        })
        .map_err(|e| format!("Failed to query minute activity: {e}（查询分钟活动失败）"))?;
    for row in rows {
        out.push(
            row.map_err(|e| format!("Failed to query minute activity: {e}（查询分钟活动失败）"))?,
        );
    }
    Ok(out)
}

// ─── C. get_timeline / get_top_apps ────────────────────────────────────────

/// 规范化时间边界：裸日期 `YYYY-MM-DD` 按**本地日界**展开（与 dash 的
/// local_day_range 同语义，注意 mcp 进程 TZ）——from 当日本地 00:00，
/// to 当日本地日末（= 次日本地 00:00，[start,end) 语义，不多吞一天）。
/// RFC3339 等完整时间戳统一解析后转成 **UTC 规范形**（`+00:00` 后缀、秒精度）
/// 再进 SQL：timestamp 列全部由 `DateTime<Utc>::to_rfc3339()` 写入（`+00:00` 形），
/// 边界若原样透传 `+08:00` 等显式偏移字面量，RFC3339 **字符串比较**的字典序
/// 将不等于时间序（如 `+08:00` < `+00:00` 字典序为假的时间序），导致边界漏/多事件。
/// 解析失败（非 RFC3339 且非裸日期）时报可读错误（与 get_summary 的日期错误
/// 同约定），不再静默回退字符串比较——静默回退会让边界按字典序进 SQL，结果
/// 错误且无提示。
pub(crate) fn normalize_bound(v: &str, is_to: bool) -> Result<String, String> {
    let b = v.as_bytes();
    if b.len() == 10
        && b[4] == b'-'
        && b[7] == b'-'
        && chrono::NaiveDate::parse_from_str(v, "%Y-%m-%d").is_ok()
    {
        if let Some((s, e)) = queries::local_day_range(v) {
            // from → 当日本地 00:00；to → 当日本地日末（即次日 00:00）
            return Ok(if is_to { e } else { s });
        }
    }
    match chrono::DateTime::parse_from_rfc3339(v) {
        Ok(t) => {
            let t = t.with_timezone(&Utc);
            // to 边界带小数秒时**向上取整到秒**：向下截断会把 [floor(t), t)
            // 之间的事件排除在外（to 是排他上界），漏掉本应属于窗口的段；
            // from 是包含下界，截断即可，不受影响。
            let t = if is_to && t.timestamp_subsec_nanos() > 0 {
                t + chrono::Duration::seconds(1)
            } else {
                t
            };
            Ok(t.to_rfc3339_opts(chrono::SecondsFormat::Secs, false))
        }
        Err(_) => Err(format!(
            "Bad date format: {}, expected RFC3339 or YYYY-MM-DD（时间格式错，应为 RFC3339 或 YYYY-MM-DD）",
            trunc_echo(v, ECHO_MAX_CHARS)
        )),
    }
}

/// 应用/窗口时间线段。granularity=minute 逐段返回；hour 把同一本地小时内
/// 连续同应用段合并。返回完整 JSON：segments + total_segments + truncated。
///
/// 截断与分页：按事件时间升序构建，超限时保留**最新** limit 段（丢弃的是
/// 最旧段，truncation="oldest-dropped"）——符合"我刚才在用什么"的主要查询
/// 意图。truncated=true 时响应带 `next_from`（保留段最早一段的 start）；
/// 分页推进规则：客户端下一页以**同一 from、to=next_from** 再查一次即可
/// 无缝续拉更早的段（段为 [start,end) 半开区间，next_from 恰是已取内容的
/// 排他下界），循环直到 truncated=false，各页拼接不重不漏。段 start 严格
/// 递增（同 timestamp 的重复 switch 在构建时去重），客户端按 start 推进
/// 游标不会死循环。
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
    let from = normalize_bound(from, false)?;
    let to = normalize_bound(to, true)?;
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
    // 零时长段防御：同一 timestamp 连发两次 switch（如双事件同毫秒落库）会
    // 产生 start 相同的零时长段，按段 start/end 推进游标的客户端会原地死循环。
    // 去重：同 timestamp 只保留最后一个 switch（该时刻之后的前台应用以后者为准），
    // 保证段 start 严格递增。
    let mut rows_deduped: Vec<&(String, String)> = Vec::with_capacity(rows.len());
    for (i, row) in rows.iter().enumerate() {
        if rows
            .get(i + 1)
            .map(|(next_ts, _)| next_ts == &row.0)
            .unwrap_or(false)
        {
            continue;
        }
        rows_deduped.push(row);
    }

    let mut segs: Vec<(String, String, String)> = Vec::new(); // (app,start,end)
    for (i, (ts, app)) in rows_deduped.iter().enumerate() {
        let end = rows_deduped
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
    // 保留最新 limit 段（不是 take(limit) 的最旧段）：主要查询意图是"最近在用什么"
    let skip = total_segments.saturating_sub(limit);
    let next_from = if truncated {
        Some(segs[skip].1.clone())
    } else {
        None
    };
    let out: Vec<Value> = segs[skip..]
        .iter()
        .map(|(app, start, end)| {
            let dur = chrono::DateTime::parse_from_rfc3339(start)
                .ok()
                .zip(chrono::DateTime::parse_from_rfc3339(end).ok())
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
        "next_from": next_from.map(Value::String).unwrap_or(Value::Null),
    }))
}

/// 窗口 [from,to) 内各前台应用驻留秒数，降序返回（get_top_apps 数据面）。
/// 驻留口径与 timeline 相同：switch 事件起点到下一 switch（末段到 to）。
pub fn top_apps(conn: &Connection, from: &str, to: &str, limit: usize) -> Result<Value, String> {
    let from = normalize_bound(from, false)?;
    let to = normalize_bound(to, true)?;
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
///
/// `bridge_minutes` 固定为 core 的 DEFAULT_PRESENCE_BRIDGE_MINUTES（2）：
/// MCP 数据面恒用默认桥接值现算，面板走用户 settings 里可能改过的值，
/// 两处数字可以不同——响应显式带回本工具实际使用的值，避免客户端误以为
/// 与面板展示必然一致。
pub fn anomalies(conn: &Connection, days: usize, limit: usize) -> Value {
    let days = days.clamp(1, 30);
    let limit = clamp_limit(Some(limit));
    let bridge_minutes = kynoptic_core::constants::DEFAULT_PRESENCE_BRIDGE_MINUTES as i64;
    let mut out: Vec<Value> = Vec::new();
    for i in 0..days {
        let date = queries::date_offset_str(-(i as i64));
        let list = kynoptic_core::anomaly::detect_all(conn, &date).unwrap_or_default();
        for a in list {
            if out.len() >= limit {
                return json!({
                    "anomalies": out,
                    "truncated": true,
                    "bridge_minutes": bridge_minutes,
                });
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
    json!({
        "anomalies": out,
        "truncated": false,
        "bridge_minutes": bridge_minutes,
    })
}

// ─── E. wait_for ────────────────────────────────────────────────────────────

/// 检查信号当前是否成立（纯读取，单次）。
pub fn check_signal(conn: &Connection, signal: &str) -> Result<bool, String> {
    if !SIGNALS.contains(&signal) {
        return Err(format!(
            "Unknown signal: {} (allowed: {})（未知信号，允许: {}）",
            trunc_echo(signal, ECHO_MAX_CHARS),
            SIGNALS.join("/"),
            SIGNALS.join("/")
        ));
    }
    Ok(match signal {
        "late_night" => {
            let hour = Local::now().hour();
            // 与 anomaly::detect 的 23:00-06:00 窗口同口径（双边界），
            // 不能只判 >= 23——凌晨 0-5 点同样是深夜窗口
            !(kynoptic_core::constants::LATE_NIGHT_END_HOUR
                ..kynoptic_core::constants::LATE_NIGHT_HOUR_START)
                .contains(&hour)
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
                    disks.iter().any(|d| {
                        d.get("used_percent")
                            .and_then(|v| v.as_f64())
                            .unwrap_or(0.0)
                            >= 90.0
                    })
                })
                .unwrap_or(false)
        }
        "marathon_session" => {
            let date = queries::today_local_str();
            let Some((s, e)) = queries::local_day_range(&date) else {
                return Ok(false);
            };
            let minutes: Vec<String> = minute_activity(conn, &s, &e)?
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
/// `shutdown` 为 stdin EOF 关停信号（serve 退出读循环时置位）：轮询循环每次
/// 醒来检查一次，让后台线程数秒内退出而非阻塞满 1800s 拖住 join。
pub fn wait_for(
    conn: &Connection,
    signal: &str,
    timeout_sec: u64,
    shutdown: &std::sync::atomic::AtomicBool,
) -> Result<Value, String> {
    let timeout = timeout_sec.clamp(1, 1800);
    let start = std::time::Instant::now();
    let poll = std::time::Duration::from_secs(2);
    loop {
        if shutdown.load(std::sync::atomic::Ordering::Relaxed) {
            return Ok(json!({
                "signal": signal,
                "status": "shutdown",
                "elapsed_sec": start.elapsed().as_secs(),
            }));
        }
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
        // limit 钳制：limit=1 → 只回**最新** 1 段（app b），带 total_segments
        // + truncation 策略 + next_from（下一页游标 = 保留段最早 start）
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
        assert_eq!(v["next_from"], json!("2026-09-09T01:10:00+00:00"));
        let arr = v["segments"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["app"], json!("b"), "截断必须保留最新段而非最旧段");
        // 未截断时 next_from 为 null
        let v = timeline(
            &conn,
            "2026-09-09T00:00:00+00:00",
            "2026-09-09T02:00:00+00:00",
            "minute",
            20,
        )
        .unwrap();
        assert_eq!(v["next_from"], Value::Null);
    }

    /// 分页拼接：513 段（含同 timestamp 双 switch 产生的零时长段场景）
    /// 两页拉取不重不漏；段 start 严格递增，按游标推进不会死循环。
    #[test]
    fn timeline_pagination_two_pages_covers_all_without_overlap() {
        let conn = mem_conn();
        let base = chrono::DateTime::parse_from_rfc3339("2026-09-09T00:00:00+00:00")
            .unwrap()
            .with_timezone(&Utc);
        let ts = |i: i64| (base + chrono::Duration::minutes(i)).to_rfc3339();
        // 513 个 switch；其中 i=200 处同 timestamp 插两次（零时长段场景，去重后仍 513 段）
        for i in 0..513 {
            insert(
                &conn,
                &ts(i),
                "window",
                "switch",
                Some(&format!("app{}", i % 7)),
                None,
            );
        }
        insert(&conn, &ts(200), "window", "switch", Some("dup"), None);
        let to = ts(600);
        // 第 1 页：保留最新 300 段
        let p1 = timeline(&conn, &ts(0), &to, "minute", 300).unwrap();
        assert_eq!(p1["total_segments"], json!(513));
        assert_eq!(p1["truncated"], json!(true));
        let s1 = p1["segments"].as_array().unwrap();
        assert_eq!(s1.len(), 300);
        assert_eq!(s1[0]["start"], json!(ts(213)), "必须保留最新段，丢最旧段");
        // 第 2 页：同一 from、to = next_from
        let next_from = p1["next_from"].as_str().unwrap().to_string();
        assert_eq!(next_from, ts(213), "next_from = 保留段最早 start");
        let p2 = timeline(&conn, &ts(0), &next_from, "minute", 300).unwrap();
        assert_eq!(p2["truncated"], json!(false));
        let s2 = p2["segments"].as_array().unwrap();
        assert_eq!(s2.len(), 213);
        // 拼接不重不漏：start 严格递增、共 513 段
        let mut starts: Vec<&str> = s1
            .iter()
            .chain(s2.iter())
            .map(|s| s["start"].as_str().unwrap())
            .collect();
        assert_eq!(starts.len(), 513);
        starts.sort_unstable();
        let n = starts.len();
        starts.dedup();
        assert_eq!(starts.len(), n, "段 start 不得重复（零时长段已合并/去重）");
        for w in starts.windows(2) {
            assert!(w[0] < w[1], "段 start 必须严格递增: {} vs {}", w[0], w[1]);
        }
        // 全量一次查询的段序与分页拼接一致（抽查首尾）
        let all = timeline(&conn, &ts(0), &to, "minute", 1000).unwrap();
        let sa = all["segments"].as_array().unwrap();
        assert_eq!(sa.len(), 513);
        assert_eq!(sa[0]["start"], json!(starts[0]));
        assert_eq!(sa[512]["start"], json!(starts[512]));
    }

    /// 同 timestamp 双 switch 不产生零时长段（构建时去重）。
    #[test]
    fn timeline_dedupes_same_timestamp_switches() {
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
            "2026-09-09T01:30:00+00:00",
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
            Some("c"),
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
        let arr = v["segments"].as_array().unwrap();
        assert_eq!(
            arr.len(),
            2,
            "同 timestamp 重复 switch 不得产生零时长段: {v}"
        );
        for s in arr {
            assert!(s["duration_sec"].as_i64().unwrap() > 0, "{s}");
        }
        assert_eq!(
            arr[1]["app"],
            json!("c"),
            "同 timestamp 保留最后一个 switch"
        );
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

    /// 带显式偏移的边界必须按**时刻**而非字符串比较：to=本地(UTC+8) 9/17 00:00
    /// 即 16:00Z，之后的 16:30Z 事件不得混入；Z 与 +00:00 混用同样成立。
    #[test]
    fn timeline_explicit_offsets_compare_by_instant() {
        let conn = mem_conn();
        insert(
            &conn,
            "2026-09-16T15:30:00+00:00",
            "window",
            "switch",
            Some("in"),
            None,
        );
        insert(
            &conn,
            "2026-09-16T16:30:00+00:00",
            "window",
            "switch",
            Some("out"),
            None,
        );
        let v = timeline(
            &conn,
            "2026-09-16T08:00:00+08:00",
            "2026-09-17T00:00:00+08:00",
            "minute",
            10,
        )
        .unwrap();
        let arr = v["segments"].as_array().unwrap();
        assert_eq!(arr.len(), 1, "to(+08:00)=16:00Z 之后的事件不得混入: {v}");
        assert_eq!(arr[0]["app"], json!("in"));
        // from 用 Z、to 用 +00:00 混排：两事件都在窗口内
        let v = top_apps(
            &conn,
            "2026-09-16T15:00:00Z",
            "2026-09-16T17:00:00+00:00",
            10,
        )
        .unwrap();
        assert_eq!(v["total_apps"], json!(2));
        // 边界倒挂（按时刻）：16:00Z 之后不能作为 to 排在 from 前，应报可读错误
        assert!(timeline(
            &conn,
            "2026-09-17T18:00:00+08:00", // 10:00Z，晚于 to
            "2026-09-17T09:00:00Z",
            "minute",
            10
        )
        .is_err());
    }

    /// Z 形输入规范化为 `+00:00` 形（与库内 timestamp 存储格式一致）；
    /// 非 RFC3339 报可读错误（审查：静默回退字符串比较会让边界按字典序
    /// 进 SQL，结果错误且无提示）。
    #[test]
    fn normalize_bound_canonicalizes_offsets() {
        assert_eq!(
            normalize_bound("2026-09-17T00:00:00Z", true).unwrap(),
            "2026-09-17T00:00:00+00:00"
        );
        assert_eq!(
            normalize_bound("2026-09-17T00:00:00+08:00", true).unwrap(),
            "2026-09-16T16:00:00+00:00"
        );
        // 带小数秒 → from 截到秒精度（与库内整秒字面量字典序可比）
        assert_eq!(
            normalize_bound("2026-09-17T00:00:00.123+08:00", false).unwrap(),
            "2026-09-16T16:00:00+00:00"
        );
        // to 带小数秒 → **向上取整到秒**（to 是排他上界，截断会漏段）
        assert_eq!(
            normalize_bound("2026-09-17T00:00:00.123+08:00", true).unwrap(),
            "2026-09-16T16:00:01+00:00"
        );
        assert_eq!(
            normalize_bound("2026-09-17T00:00:00.999Z", true).unwrap(),
            "2026-09-17T00:00:01+00:00"
        );
        // to 整秒不变
        assert_eq!(
            normalize_bound("2026-09-17T00:00:00Z", true).unwrap(),
            "2026-09-17T00:00:00+00:00"
        );
        // 解析失败 → 可读错误（与 get_summary 的日期错误同约定）
        let err = normalize_bound("garbage", false).unwrap_err();
        assert!(err.contains("Bad date format"), "{err}");
        // 裸日期路径不变
        let (s, e) = queries::local_day_range("2026-09-17").unwrap();
        assert_eq!(normalize_bound("2026-09-17", false).unwrap(), s);
        assert_eq!(normalize_bound("2026-09-17", true).unwrap(), e);
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

    /// late_night 信号必须与 anomaly::detect 的 23:00-06:00 窗口同口径
    /// （双边界）：此前只判 hour >= 23，凌晨 0-5 点漏判。
    #[test]
    fn late_night_signal_matches_detect_window() {
        let conn = mem_conn();
        let hour = Local::now().hour();
        let expected = !(kynoptic_core::constants::LATE_NIGHT_END_HOUR
            ..kynoptic_core::constants::LATE_NIGHT_HOUR_START)
            .contains(&hour);
        assert_eq!(check_signal(&conn, "late_night").unwrap(), expected);
    }

    /// get_anomalies 响应必须带 bridge_minutes（MCP 用 core 默认桥接值 2）。
    #[test]
    fn anomalies_report_bridge_minutes() {
        let conn = mem_conn();
        let v = anomalies(&conn, 1, 20);
        assert_eq!(
            v["bridge_minutes"],
            json!(kynoptic_core::constants::DEFAULT_PRESENCE_BRIDGE_MINUTES as i64)
        );
    }

    #[test]
    fn wait_for_times_out() {
        let conn = mem_conn();
        let stop = std::sync::atomic::AtomicBool::new(false);
        let v = wait_for(&conn, "thermal_hot", 1, &stop).unwrap();
        assert_eq!(v["status"], json!("timeout"));
        assert_eq!(v["timeout_sec"], json!(1));
    }

    /// 审查 P2：EOF 关停信号置位后，wait_for 必须在下一轮询点立即退出
    ///（status=shutdown），不得阻塞满 timeout。
    #[test]
    fn wait_for_exits_promptly_on_shutdown() {
        let conn = mem_conn();
        let stop = std::sync::atomic::AtomicBool::new(true);
        let v = wait_for(&conn, "thermal_hot", 1800, &stop).unwrap();
        assert_eq!(v["status"], json!("shutdown"));
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
        let stop = std::sync::atomic::AtomicBool::new(false);
        assert!(wait_for(&conn, "bogus", 1, &stop).is_err());
    }
}
