//! kynoptic-dash — 本机只读 dashboard HTTP 服务（cli 与 tray 共用唯一实现）。
//!
//! 从 `crates/cli/src/dashboard.rs` 迁出（原为 bin 私有模块，tray 曾整份复制，
//! 属技术债；现统一为 lib crate，`kynoptic-ctl dashboard` 与 `kynoptic-tray`
//! 均调用 [`serve`]）。绑定 127.0.0.1 的极简 HTTP 服务（std::net 手写 handler，
//! 零新依赖、零 CDN，静态页 `dashboard.html` 经 `include_str!` 内嵌，完全离线）。
//!
//! **只读铁律**：数据库经 `kynoptic_mcp::state::open_reader`（SQLITE_OPEN_READ_ONLY）
//! 打开——不建表、不跑迁移、永不写 events；agg 缓存只经采集器/维护路径写。
//! tray 的采集器在同进程持有写连接：READ_ONLY 连接对 WAL 库天然兼容（多读一写），
//! 仅在非 WAL 库上回退纯 READ_ONLY 连接 + busy_timeout（照搬原 cli 策略）。
//! 唯一写路径是 POST /api/settings（写 settings.json，不触碰 events）。
//!
//! 端点（除 POST /api/settings 外全部 GET、JSON、只读）：
//! - `/`                             内嵌双语单页（Overview/Activity/Anomalies/Settings）
//! - `/api/summary?date=`            当日 active_minutes / keys / clicks / top_app
//! - `/api/timeline?hours=`          近 N 小时按本地小时桶的应用分布（top5 + other）
//! - `/api/anomalies?days=`          复用 MCP `get_anomalies` 的同一异常检测
//! - `/api/status`                   今日日期 + 最新事件时间戳 + db 路径
//! - `/api/overview`                 本次会话 uptime / 今日事件数 / 启用监控器数 /
//!   DB 大小 / CPU / 内存 / 前台应用（复用 MCP `get_current_status` 同一数据面）
//! - `/api/heatmap?weeks=`           按本地日聚合的活跃度 `[{date,value}]`，缺数天补零
//! - `/api/apps?days=`               Top 应用排行（window 事件，空名排除）
//! - `/api/hours?date=`              指定日 24 小时逐时活动量（缺时补零）
//! - `/api/settings` (GET/POST)      设置读写（写 settings.json，不触碰 events）
//!
//! 无鉴权：仅绑定回环地址，不暴露到网络（页脚已声明）。

pub mod settings;

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;

use chrono::{DateTime, Local, Utc};
use rusqlite::{params, Connection};
use serde_json::{json, Value};

use kynoptic_core::queries;
use kynoptic_core::registry;
use kynoptic_core::{Error, Result};

use settings::AppSettings;

/// 内嵌静态页（与产品 monospace/终端风一致的暗色双语单页）。
pub const DASHBOARD_HTML: &str = include_str!("dashboard.html");

pub const DEFAULT_PORT: u16 = settings::DEFAULT_DASHBOARD_PORT;

// ─── 数据面（&Connection / &Path 纯函数，可脱离 TCP 单测） ───────────────────

/// GET /api/summary?date= — 当日四卡数据。数字全部来自 DB。
pub fn api_summary(conn: &Connection, date: &str) -> std::result::Result<Value, String> {
    let (start, end) = queries::local_day_range(date)
        .ok_or_else(|| format!("日期格式错: {date}（应为 YYYY-MM-DD）"))?;
    let totals = queries::day_totals(conn, date);
    let top_app = queries::top_apps_today(conn, &start, &end, 1)
        .into_iter()
        .next()
        .map(|(app, _)| app);
    Ok(json!({
        "date": date,
        "active_minutes": totals.active_minutes,
        "keys": totals.keys,
        "clicks": totals.clicks,
        "top_app": top_app,
    }))
}

/// 单小时桶内的应用事件分布，top 5 + 其余归并 other。
fn top5_with_other(mut apps: Vec<(String, i64)>) -> Vec<(String, i64)> {
    apps.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    if apps.len() <= 5 {
        return apps;
    }
    let other: i64 = apps[5..].iter().map(|(_, n)| n).sum();
    apps.truncate(5);
    apps.push(("(other)".into(), other));
    apps
}

/// GET /api/timeline?hours= — 近 `hours` 本地小时的事件分布，按小时桶。
/// `now` 注入以便硬件无关测试。只含有数据的桶，桶内应用 top5 + other。
pub fn api_timeline_at(
    conn: &Connection,
    hours: u32,
    now: DateTime<Utc>,
) -> std::result::Result<Value, String> {
    let hours = hours.clamp(1, 48);
    // 边界按 UTC 计算后直接用于 WHERE（timestamp 列为 UTC RFC3339）
    let end = now;
    let start = now - chrono::Duration::hours(i64::from(hours));
    let off = local_offset_modifier_at(now);

    let mut stmt = conn
        .prepare(
            "SELECT substr(datetime(timestamp, ?1), 1, 13) AS hour_bucket, \
                    COALESCE(NULLIF(app_name, ''), window_title, '(unknown)') AS app, \
                    COUNT(*) AS cnt \
             FROM events \
             WHERE timestamp >= ?2 AND timestamp < ?3 \
               AND event_type IN ('keyboard','mouse','window') \
             GROUP BY hour_bucket, app",
        )
        .map_err(|e| e.to_string())?;
    let rows: Vec<(String, String, i64)> = stmt
        .query_map(params![&off, start.to_rfc3339(), end.to_rfc3339()], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
            ))
        })
        .map_err(|e| e.to_string())?
        .flatten()
        .collect();

    // (本地小时桶 "YYYY-MM-DDTHH", [(app, cnt)])
    let mut by_bucket: std::collections::BTreeMap<String, Vec<(String, i64)>> =
        std::collections::BTreeMap::new();
    for (bucket, app, cnt) in rows {
        // SQLite datetime() 输出 "YYYY-MM-DD HH"；统一成 ISO "YYYY-MM-DDTHH"
        let bucket = bucket.replacen(' ', "T", 1);
        by_bucket.entry(bucket).or_default().push((app, cnt));
    }
    let buckets: Vec<Value> = by_bucket
        .into_iter()
        .map(|(hour, apps)| {
            let list: Vec<Value> = top5_with_other(apps)
                .into_iter()
                .map(|(app, n)| json!({"app": app, "events": n}))
                .collect();
            json!({"hour": hour, "apps": list})
        })
        .collect();
    Ok(json!({"hours": hours, "generated_at": now.to_rfc3339(), "buckets": buckets}))
}

/// 指定时刻的本地偏移修饰符（与 `queries::local_offset_modifier` 同口径，
/// 但允许测试注入时刻；当前时刻下两者一致）。
fn local_offset_modifier_at(now: DateTime<Utc>) -> String {
    let secs = now.with_timezone(&Local).offset().local_minus_utc() as i64;
    format!(
        "{}{} seconds",
        if secs >= 0 { "+" } else { "-" },
        secs.abs()
    )
}

/// GET /api/anomalies?days= — 复用 MCP get_anomalies 的同一检测入口。
pub fn api_anomalies(conn: &Connection, days: u32) -> Value {
    kynoptic_mcp::state::anomalies(conn, days as usize, 100)
}

/// GET /api/status — 今日日期 + 最新事件时间戳（采集器存活的保守代理）。
pub fn api_status(conn: &Connection, db_path: &Path) -> Value {
    json!({
        "today": queries::today_local_str(),
        "last_event_ts": queries::latest_event_ts(conn),
        "db_path": db_path.display().to_string(),
        "bind": "127.0.0.1",
        "read_only": true,
    })
}

/// GET /api/overview — 运行信息卡片数据。
///
/// 会话：sessions 表最近一条（end_time IS NULL 视为进行中）；
/// 系统信息复用 MCP `get_current_status` 同一数据面（current_state 表优先，
/// 缺失时从最新 system/heartbeat 与 window/switch 事件推导）。
pub fn api_overview(conn: &Connection, db_path: &Path) -> Value {
    let today = queries::today_local_str();
    let (start, end) = queries::today_range();
    let today_events = queries::count_today_events(conn, &start, &end);
    let s = settings::load(db_path);

    let (session_started_at, session_open, uptime_seconds) = match conn
        .query_row(
            "SELECT start_time, end_time FROM sessions ORDER BY id DESC LIMIT 1",
            [],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)),
        )
        .ok()
    {
        Some((st, end)) => {
            let uptime = chrono::DateTime::parse_from_rfc3339(&st)
                .ok()
                .map(|t| (Utc::now() - t.with_timezone(&Utc)).num_seconds().max(0));
            (
                Value::from(st),
                Value::from(end.is_none()),
                uptime.map(Value::from).unwrap_or(Value::Null),
            )
        }
        None => (Value::Null, Value::Null, Value::Null),
    };

    // DB 大小：主文件 + WAL（metadata，不碰内容）
    let db_size_bytes = std::fs::metadata(db_path).map(|m| m.len()).unwrap_or(0);
    let db_wal_bytes = std::fs::metadata(db_path_with_wal(db_path))
        .map(|m| m.len())
        .unwrap_or(0);

    // 系统信息：与 MCP get_current_status 同一实现（cpu/mem/前台应用）
    let sys = kynoptic_mcp::state::current_status(conn, None).unwrap_or_else(|_| json!({}));

    json!({
        "today": today,
        "today_events": today_events,
        "monitors_enabled": s.enabled_monitors.len(),
        "monitors_total": registry::MONITOR_REGISTRY.len(),
        "input_counts_only": s.input_counts_only,
        "db_size_bytes": db_size_bytes,
        "db_wal_bytes": db_wal_bytes,
        "session_started_at": session_started_at,
        "session_open": session_open,
        "uptime_seconds": uptime_seconds,
        "cpu_pct": sys.get("cpu_pct").cloned().unwrap_or(Value::Null),
        "mem_pct": sys.get("mem_pct").cloned().unwrap_or(Value::Null),
        "foreground_app": sys.get("foreground_app").cloned().unwrap_or(Value::Null),
    })
}

fn db_path_with_wal(db_path: &Path) -> std::path::PathBuf {
    let mut s = db_path.as_os_str().to_os_string();
    s.push("-wal");
    std::path::PathBuf::from(s)
}

/// GET /api/heatmap?weeks= — 近 `weeks` 周按本地日聚合的活跃度（事件总量）。
/// 返回 `[{date, value}]`，按日升序，缺数天补零。`now` 注入以便测试。
pub fn api_heatmap_at(conn: &Connection, weeks: u32, today: chrono::NaiveDate) -> Value {
    let weeks = weeks.clamp(1, 52);
    let days = i64::from(weeks) * 7;
    let since = today - chrono::Duration::days(days - 1);
    let since_utc = queries::local_day_range(&since.format("%Y-%m-%d").to_string())
        .map(|(s, _)| s)
        .unwrap_or_default();
    let mut by_date: std::collections::BTreeMap<String, i64> =
        queries::daily_counts_since(conn, &since_utc)
            .into_iter()
            .collect();
    let mut out = Vec::new();
    let mut d = since;
    while d <= today {
        let key = d.format("%Y-%m-%d").to_string();
        let value = by_date.remove(&key).unwrap_or(0);
        out.push(json!({"date": key, "value": value}));
        d += chrono::Duration::days(1);
    }
    json!({"weeks": weeks, "days": out})
}

/// GET /api/apps?days= — 近 `days` 天（含今日）window 事件 Top 应用排行。
/// 应用名取 COALESCE(NULLIF(app_name,''), window_title)（采集器把可读名写进
/// window_title 而 app_name 常为空，与 MCP foreground_app 同一降敏约定），
/// 空名排除。`now` 注入以便测试。
pub fn api_apps_at(conn: &Connection, days: u32, today: chrono::NaiveDate) -> Value {
    let days = days.clamp(1, 365);
    let since_date = today - chrono::Duration::days(i64::from(days) - 1);
    let since = queries::local_day_range(&since_date.format("%Y-%m-%d").to_string())
        .map(|(s, _)| s)
        .unwrap_or_default();
    let stmt = conn
        .prepare(
            "SELECT COALESCE(NULLIF(app_name,''), window_title, '') AS app, COUNT(*) AS cnt \
             FROM events \
             WHERE event_type = 'window' AND timestamp >= ?1 AND app <> '' \
             GROUP BY app ORDER BY cnt DESC, app ASC LIMIT 10",
        )
        .map_err(|e| e.to_string());
    let apps: Vec<Value> = match stmt {
        Ok(mut s) => s
            .query_map(params![&since], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
            })
            .map_err(|e| e.to_string())
            .map(|rows| {
                rows.flatten()
                    .map(|(app, cnt)| json!({"app": app, "count": cnt}))
                    .collect::<Vec<Value>>()
            })
            .unwrap_or_default(),
        Err(_) => Vec::new(),
    };
    json!({"days": days, "apps": apps})
}

/// GET /api/hours?date= — 指定本地日 24 小时逐时活动量（事件总数），缺时补零。
pub fn api_hours(conn: &Connection, date: &str) -> std::result::Result<Value, String> {
    let date = match date {
        "" | "today" => queries::today_local_str(),
        d => d.to_string(),
    };
    let (start, end) = queries::local_day_range(&date)
        .ok_or_else(|| format!("日期格式错: {date}（应为 YYYY-MM-DD 或 today）"))?;
    let mut map = [0i64; 24];
    for (hour, cnt) in queries::hourly_counts_today(conn, &start, &end) {
        if (0..24).contains(&hour) {
            map[hour as usize] = cnt;
        }
    }
    Ok(json!({"date": date, "values": map}))
}

/// GET /api/input?days= — 输入统计聚合（近 `days` 天，含今日）。
///
/// 数据源是 input_agg 分钟计数行（keyboard 行含 per-key 频次 `vk` map，
/// mouse 行含分键点击/滚轮/移动距离）。只读聚合，缺天补零。
/// `now` 注入以便测试。
pub fn api_input_at(conn: &Connection, days: u32, today: chrono::NaiveDate) -> std::result::Result<Value, String> {
    let days = days.clamp(1, 365);
    let since_date = today - chrono::Duration::days(i64::from(days) - 1);
    let since = queries::local_day_range(&since_date.format("%Y-%m-%d").to_string())
        .map(|(s, _)| s)
        .unwrap_or_default();
    let mut stmt = conn
        .prepare(
            "SELECT substr(datetime(timestamp, ?1), 1, 13) AS hour_bucket, event_type, event_data \
             FROM events \
             WHERE event_action = 'input_agg' AND timestamp >= ?2",
        )
        .map_err(|e| e.to_string())?;
    let off = {
        let secs = Local::now().offset().local_minus_utc() as i64;
        format!("{}{} seconds", if secs >= 0 { "+" } else { "-" }, secs.abs())
    };
    let rows: Vec<(String, String, Option<String>)> = stmt
        .query_map(params![&off, &since], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })
        .map_err(|e| e.to_string())?
        .flatten()
        .collect();

    #[derive(Default)]
    struct Totals {
        keys: u64,
        clicks: u64,
        left: u64,
        right: u64,
        middle: u64,
        side1: u64,
        side2: u64,
        scroll_ticks: u64,
        moves: u64,
        dist_px: u64,
    }
    let mut totals = Totals::default();
    let mut key_freq: std::collections::BTreeMap<String, u64> = std::collections::BTreeMap::new();
    let mut series: std::collections::BTreeMap<String, (u64, u64)> = std::collections::BTreeMap::new();
    let today_prefix = today.format("%Y-%m-%d").to_string();
    let mut hourly_today: [u64; 24] = [0; 24];

    for (bucket, etype, data) in rows {
        let v: Value = match data.as_deref().and_then(|s| serde_json::from_str(s).ok()) {
            Some(v) => v,
            None => continue,
        };
        let num = |k: &str| v.get(k).and_then(|x| x.as_u64()).unwrap_or(0);
        let day: String = bucket.chars().take(10).collect();
        let e = series.entry(day).or_default();
        // 今日逐时输入量（keys+clicks），供"输入节奏"条形图
        if bucket.starts_with(&today_prefix) {
            if let Ok(h) = bucket.get(11..13).unwrap_or("").parse::<usize>() {
                if h < 24 {
                    if etype == "keyboard" {
                        hourly_today[h] += num("keys");
                    } else if etype == "mouse" {
                        hourly_today[h] += num("clicks");
                    }
                }
            }
        }
        if etype == "keyboard" {
            let keys = num("keys");
            totals.keys += keys;
            e.0 += keys;
            if let Some(map) = v.get("vk").and_then(|x| x.as_object()) {
                for (k, n) in map {
                    if let Some(n) = n.as_u64() {
                        *key_freq.entry(k.clone()).or_default() += n;
                    }
                }
            }
        } else if etype == "mouse" {
            let clicks = num("clicks");
            totals.clicks += clicks;
            totals.left += num("clicks_left");
            totals.right += num("clicks_right");
            totals.middle += num("clicks_middle");
            totals.side1 += num("clicks_side1");
            totals.side2 += num("clicks_side2");
            totals.scroll_ticks += num("scroll_ticks");
            totals.moves += num("moves");
            totals.dist_px += num("move_distance_px");
            e.1 += clicks;
        }
    }

    let days_out: Vec<Value> = series
        .iter()
        .map(|(d, (keys, clicks))| json!({"date": d, "keys": keys, "clicks": clicks}))
        .collect();
    // 最近一次输入设备拓扑快照（device_snapshot.input_devices，仅拓扑变化时写入）
    let input_devices: Vec<Value> = conn
        .query_row(
            "SELECT event_data FROM events              WHERE event_action = 'device_snapshot' AND json_extract(event_data, '$.input_devices') IS NOT NULL              ORDER BY id DESC LIMIT 1",
            [],
            |r| r.get::<_, Option<String>>(0),
        )
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s?).ok())
        .and_then(|v| v.get("input_devices").cloned())
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default();
    Ok(json!({
        "days": days,
        "input_devices": input_devices,
        "keys_total": totals.keys,
        "clicks_total": totals.clicks,
        "clicks_left": totals.left,
        "clicks_right": totals.right,
        "clicks_middle": totals.middle,
        "clicks_side1": totals.side1,
        "clicks_side2": totals.side2,
        "hourly_today": hourly_today,
        "scroll_ticks": totals.scroll_ticks,
        "moves": totals.moves,
        "move_distance_px": totals.dist_px,
        "key_freq": key_freq,
        "series": days_out,
    }))
}

/// GET /api/report?date= — 单日报告：色带时间轴、类别占比、专注时段。
///
/// dwell 分段：window/switch 事件间隔即上一应用的停留时长；单段上限 120 分钟
/// （离开电脑时的尾段不无限延长）。专注块：间隔 ≤5 分钟的连续活动且总长 ≥20 分钟。
pub fn api_report_at(conn: &Connection, date: &str, s: &settings::AppSettings) -> std::result::Result<Value, String> {
    let date = match date {
        "" | "today" => queries::today_local_str(),
        d => d.to_string(),
    };
    let (start, end) = queries::local_day_range(&date)
        .ok_or_else(|| format!("日期格式错: {date}（应为 YYYY-MM-DD 或 today）"))?;
    let mut stmt = conn
        .prepare(
            "SELECT timestamp, COALESCE(NULLIF(app_name,''), window_title, '(unknown)') AS app, window_title              FROM events              WHERE event_type = 'window' AND event_action = 'switch'                AND timestamp >= ?1 AND timestamp < ?2              ORDER BY timestamp",
        )
        .map_err(|e| e.to_string())?;
    let rows: Vec<(String, String, Option<String>)> = stmt
        .query_map(params![&start, &end], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })
        .map_err(|e| e.to_string())?
        .flatten()
        .collect();
    let day_start = chrono::DateTime::parse_from_rfc3339(&start)
        .map_err(|e| e.to_string())?;

    // 1) dwell 分段（分钟坐标）
    let mut segs: Vec<(String, usize, usize)> = Vec::new(); // (app, start_min, end_min)
    for (i, (ts, app, _title)) in rows.iter().enumerate() {
        let Ok(t) = chrono::DateTime::parse_from_rfc3339(ts) else { continue };
        let start_min = ((t - day_start).num_minutes().max(0) as usize).min(1439);
        let end_min = if i + 1 < rows.len() {
            match chrono::DateTime::parse_from_rfc3339(&rows[i + 1].0) {
                Ok(t2) => ((t2 - day_start).num_minutes().max(0) as usize).min(1440),
                Err(_) => start_min + 1,
            }
        } else {
            (start_min + 1).min(1440)
        };
        let end_min = end_min.min(start_min + 120); // 离开电脑的尾段封顶 2h
        if end_min <= start_min {
            continue;
        }
        // 同应用连续段合并
        if let Some(last) = segs.last_mut() {
            if last.0 == *app && start_min.saturating_sub(last.1) <= 1 {
                last.2 = end_min;
                continue;
            }
        }
        segs.push((app.clone(), start_min, end_min));
    }

    // 2) 分类
    let classify = |app: &str| -> String {
        for rule in &s.categories {
            if rule.matches(app, "") {
                return rule.name.clone();
            }
        }
        "其他".to_string()
    };
    let segments: Vec<Value> = segs
        .iter()
        .map(|(app, a, b)| {
            json!({"app": app, "category": classify(app), "start_min": a, "end_min": b})
        })
        .collect();

    // 3) 类别占比（分钟）
    let mut cat_min: std::collections::BTreeMap<String, i64> = std::collections::BTreeMap::new();
    for (app, a, b) in &segs {
        *cat_min.entry(classify(app)).or_default() += (b - a) as i64;
    }
    let categories: Vec<Value> = cat_min
        .iter()
        .map(|(name, min)| json!({"category": name, "minutes": min}))
        .collect();

    // 4) 专注块：相邻段间隙 ≤5min 合并，总长 ≥20min
    let mut blocks: Vec<(usize, usize)> = Vec::new();
    for (_, a, b) in &segs {
        if let Some(last) = blocks.last_mut() {
            if a.saturating_sub(last.1) <= 5 {
                last.1 = (*b).max(last.1);
                continue;
            }
        }
        blocks.push((*a, *b));
    }
    let focus: Vec<Value> = blocks
        .iter()
        .filter(|(a, b)| b - a >= 20)
        .map(|(a, b)| json!({"start_min": a, "end_min": b, "minutes": b - a}))
        .collect();

    Ok(json!({
        "date": date,
        "segments": segments,
        "categories": categories,
        "focus": focus,
    }))
}

/// GET /api/trends — 近 28 天每日 keys/clicks/active_minutes（来自 daily_agg 派生缓存）
/// 与 本 7 天 vs 上 7 天对比。
pub fn api_trends_at(conn: &Connection, today: chrono::NaiveDate) -> Value {
    let since_date = today - chrono::Duration::days(27);
    let since = since_date.format("%Y-%m-%d").to_string();
    let mut stmt = match conn.prepare(
        "SELECT date, keys, clicks, active_minutes FROM daily_agg WHERE date >= ?1 ORDER BY date",
    ) {
        Ok(s) => s,
        Err(_) => return json!({"daily": [], "this_week": {}, "last_week": {}}),
    };
    let rows: Vec<(String, i64, i64, i64)> = stmt
        .query_map(params![&since], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })
        .map(|rows| rows.flatten().collect())
        .unwrap_or_default();
    let daily: Vec<Value> = rows
        .iter()
        .map(|(d, k, c, m)| json!({"date": d, "keys": k, "clicks": c, "active_minutes": m}))
        .collect();
    let sum7 = |offset: usize| -> (i64, i64, i64) {
        let n = rows.len();
        let hi = n.saturating_sub(offset);
        let lo = hi.saturating_sub(7);
        rows[lo..hi].iter().fold((0, 0, 0), |acc, (_, k, c, m)| {
            (acc.0 + k, acc.1 + c, acc.2 + m)
        })
    };
    let (k1, c1, m1) = sum7(0);
    let (k0, c0, m0) = sum7(7);
    json!({
        "daily": daily,
        "this_week": {"keys": k1, "clicks": c1, "active_minutes": m1},
        "last_week": {"keys": k0, "clicks": c0, "active_minutes": m0},
    })
}

/// GET /api/apps_grid?date= — 指定本地日的"小时 × 应用"使用矩阵。
/// window 事件按本地小时桶 × 应用聚合，取当日 Top 6 应用 + 其余归并 other。
pub fn api_apps_grid_at(conn: &Connection, date: &str) -> std::result::Result<Value, String> {
    let date = match date {
        "" | "today" => queries::today_local_str(),
        d => d.to_string(),
    };
    let (start, end) = queries::local_day_range(&date)
        .ok_or_else(|| format!("日期格式错: {date}（应为 YYYY-MM-DD 或 today）"))?;
    let mut stmt = conn
        .prepare(
            "SELECT substr(datetime(timestamp, ?1), 12, 2) AS hh,                     COALESCE(NULLIF(app_name,''), window_title, '(unknown)') AS app,                     COUNT(*) AS cnt              FROM events              WHERE event_type = 'window' AND timestamp >= ?2 AND timestamp < ?3              GROUP BY hh, app",
        )
        .map_err(|e| e.to_string())?;
    let off = {
        let secs = Local::now().offset().local_minus_utc() as i64;
        format!("{}{} seconds", if secs >= 0 { "+" } else { "-" }, secs.abs())
    };
    let rows: Vec<(String, String, i64)> = stmt
        .query_map(params![&off, &start, &end], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })
        .map_err(|e| e.to_string())?
        .flatten()
        .collect();

    let mut per_app: std::collections::BTreeMap<String, i64> = std::collections::BTreeMap::new();
    let mut by_hour_app: std::collections::BTreeMap<(String, String), i64> =
        std::collections::BTreeMap::new();
    for (hh, app, cnt) in rows {
        *per_app.entry(app.clone()).or_default() += cnt;
        *by_hour_app.entry((hh, app)).or_default() += cnt;
    }
    let mut ranked: Vec<(String, i64)> = per_app.into_iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let top: Vec<String> = ranked.iter().take(6).map(|(a, _)| a.clone()).collect();
    let mut grid: std::collections::BTreeMap<String, [i64; 24]> =
        std::collections::BTreeMap::new();
    for app in &top {
        grid.insert(app.clone(), [0; 24]);
    }
    grid.insert("(other)".into(), [0; 24]);
    let mut hourly_total = [0i64; 24];
    for ((hh, app), cnt) in &by_hour_app {
        if let Ok(h) = hh.parse::<usize>() {
            if h < 24 {
                hourly_total[h] += cnt;
                let key = if top.contains(app) { app.as_str() } else { "(other)" };
                if let Some(row) = grid.get_mut(key) {
                    row[h] += cnt;
                }
            }
        }
    }
    let grid_out: Vec<Value> = grid
        .into_iter()
        .map(|(app, hours)| json!({"app": app, "hours": hours}))
        .collect();
    let totals: Vec<Value> = ranked
        .iter()
        .take(6)
        .map(|(a, n)| json!({"app": a, "count": n}))
        .collect();
    Ok(json!({"date": date, "grid": grid_out, "totals": totals, "hourly_total": hourly_total}))
}

/// GET /api/daily_top?days= — 近 `days` 天每日 Top 3 应用（window 事件）。
pub fn api_daily_top_at(conn: &Connection, days: u32, today: chrono::NaiveDate) -> Value {
    let days = days.clamp(1, 90);
    let since_date = today - chrono::Duration::days(i64::from(days) - 1);
    let since = queries::local_day_range(&since_date.format("%Y-%m-%d").to_string())
        .map(|(s, _)| s)
        .unwrap_or_default();
    let mut stmt = match conn.prepare(
        "SELECT substr(datetime(timestamp, ?1), 1, 10) AS d,                 COALESCE(NULLIF(app_name,''), window_title, '') AS app, COUNT(*) AS cnt          FROM events          WHERE event_type = 'window' AND timestamp >= ?2 AND app <> ''          GROUP BY d, app",
    ) {
        Ok(s) => s,
        Err(_) => return json!({"days": days, "days_out": []}),
    };
    let off = {
        let secs = Local::now().offset().local_minus_utc() as i64;
        format!("{}{} seconds", if secs >= 0 { "+" } else { "-" }, secs.abs())
    };
    let rows: Vec<(String, String, i64)> = stmt
        .query_map(params![&off, &since], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })
        .map(|rows| rows.flatten().collect())
        .unwrap_or_default();
    let mut by_day: std::collections::BTreeMap<String, Vec<(String, i64)>> =
        std::collections::BTreeMap::new();
    for (d, app, cnt) in rows {
        by_day.entry(d).or_default().push((app, cnt));
    }
    let days_out: Vec<Value> = by_day
        .into_iter()
        .map(|(d, mut apps)| {
            apps.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            apps.truncate(3);
            let top: Vec<Value> = apps
                .into_iter()
                .map(|(app, n)| json!({"app": app, "count": n}))
                .collect();
            json!({"date": d, "top": top})
        })
        .collect();
    json!({"days": days, "days_out": days_out})
}

/// GET /api/settings — 当前设置 + 全部监控器清单（来自 MONITOR_REGISTRY）。
pub fn api_settings(db_path: &Path) -> Value {
    let s = settings::load(db_path);
    settings_payload(&s)
}

/// 设置 + 监控器清单的统一响应体（GET 与 POST 共用）。
fn settings_payload(s: &AppSettings) -> Value {
    let monitors: Vec<Value> = registry::MONITOR_REGISTRY
        .iter()
        .map(|m| {
            json!({
                "id": m.id,
                "default_enabled": m.default_enabled,
                "sensitivity": m.sensitivity.as_str(),
            })
        })
        .collect();
    json!({
        "enabled_monitors": s.enabled_monitors,
        "autostart": s.autostart,
        "input_counts_only": s.input_counts_only,
        "dashboard_port": s.dashboard_port,
        "daily_goal_minutes": s.daily_goal_minutes,
        "categories": s.categories,
        "monitors": monitors,
    })
}

/// POST /api/settings — 接受 `enabled_monitors` / `autostart` / `dashboard_port`
/// / `input_counts_only` 任一子集；监控器 id 必须全部在注册表内，否则 400。
/// 写盘后返回新设置。
pub fn api_settings_post(db_path: &Path, body: &str) -> std::result::Result<Value, String> {
    let req: Value = serde_json::from_str(body).map_err(|e| format!("请求体不是合法 JSON: {e}"))?;
    let mut next = settings::load(db_path);
    if let Some(v) = req.get("enabled_monitors") {
        let ids: Vec<String> = v
            .as_array()
            .ok_or("enabled_monitors 应为字符串数组")?
            .iter()
            .map(|x| {
                x.as_str()
                    .map(String::from)
                    .ok_or_else(|| "enabled_monitors 应为字符串数组".to_string())
            })
            .collect::<std::result::Result<_, _>>()?;
        if let Some(bad) = settings::first_invalid_id(&ids) {
            return Err(format!("未知监控器 id: {bad}"));
        }
        next.enabled_monitors = ids;
    }
    if let Some(v) = req.get("input_counts_only") {
        next.input_counts_only = v.as_bool().ok_or("input_counts_only 应为布尔值")?;
    }
    if let Some(v) = req.get("autostart") {
        next.autostart = v.as_bool().ok_or("autostart 应为布尔值")?;
    }
    if let Some(v) = req.get("daily_goal_minutes") {
        let m = v.as_u64().ok_or("daily_goal_minutes 应为非负整数")?;
        if m > 24 * 60 {
            return Err("daily_goal_minutes 不能超过 1440".into());
        }
        next.daily_goal_minutes = m as u32;
    }
    if let Some(v) = req.get("categories") {
        let arr = v.as_array().ok_or("categories 应为数组")?;
        let mut rules = Vec::with_capacity(arr.len());
        for r in arr {
            let name = r
                .get("name")
                .and_then(|x| x.as_str())
                .ok_or("categories[].name 缺失")?
                .to_string();
            let pattern = r
                .get("pattern")
                .and_then(|x| x.as_str())
                .ok_or("categories[].pattern 缺失")?
                .to_string();
            rules.push(settings::CategoryRule { name, pattern });
        }
        next.categories = rules;
    }
    if let Some(v) = req.get("dashboard_port") {
        let port = v.as_u64().ok_or("dashboard_port 应为 0-65535 整数")?;
        if port > u16::MAX as u64 {
            return Err("dashboard_port 应为 0-65535 整数".into());
        }
        next.dashboard_port = port as u16;
    }
    settings::save(db_path, &next).map_err(|e| format!("写设置失败: {e}"))?;
    Ok(settings_payload(&next))
}

// ─── 路由表 ─────────────────────────────────────────────────────────────────

/// 路由：返回 (HTTP status, content-type, body)。
/// `path` 含 query string；`body` 仅 POST /api/settings 使用；
/// 非 GET/POST → 405；未知路径 → 404；参数坏 → 400。
pub fn route_req(
    conn: &Connection,
    method: &str,
    path: &str,
    body: &str,
    db_path: &Path,
) -> (u16, &'static str, String) {
    let (route_path, query) = match path.split_once('?') {
        Some((p, q)) => (p, q),
        None => (path, ""),
    };
    let qval = |key: &str| -> Option<String> {
        query.split('&').find_map(|kv| {
            let (k, v) = kv.split_once('=')?;
            (k == key && !v.is_empty()).then(|| v.to_string())
        })
    };

    match (method, route_path) {
        ("GET", "/") => (200, "text/html; charset=utf-8", DASHBOARD_HTML.to_string()),
        ("GET", "/api/summary") => {
            let date = qval("date").unwrap_or_else(queries::today_local_str);
            match api_summary(conn, &date) {
                Ok(v) => (200, "application/json", v.to_string()),
                Err(e) => (400, "application/json", err_json(&e)),
            }
        }
        ("GET", "/api/timeline") => {
            let hours = qval("hours")
                .and_then(|v| v.parse::<u32>().ok())
                .unwrap_or(12);
            match api_timeline_at(conn, hours, Utc::now()) {
                Ok(v) => (200, "application/json", v.to_string()),
                Err(e) => (400, "application/json", err_json(&e)),
            }
        }
        ("GET", "/api/anomalies") => {
            let days = qval("days")
                .and_then(|v| v.parse::<u32>().ok())
                .unwrap_or(7);
            (
                200,
                "application/json",
                api_anomalies(conn, days).to_string(),
            )
        }
        ("GET", "/api/status") => (
            200,
            "application/json",
            api_status(conn, db_path).to_string(),
        ),
        ("GET", "/api/overview") => (
            200,
            "application/json",
            api_overview(conn, db_path).to_string(),
        ),
        ("GET", "/api/heatmap") => {
            let weeks = qval("weeks")
                .and_then(|v| v.parse::<u32>().ok())
                .unwrap_or(12);
            (
                200,
                "application/json",
                api_heatmap_at(conn, weeks, today_naive()).to_string(),
            )
        }
        ("GET", "/api/apps") => {
            let days = qval("days")
                .and_then(|v| v.parse::<u32>().ok())
                .unwrap_or(7);
            (
                200,
                "application/json",
                api_apps_at(conn, days, today_naive()).to_string(),
            )
        }
        ("GET", "/api/hours") => {
            let date = qval("date").unwrap_or_else(|| "today".to_string());
            match api_hours(conn, &date) {
                Ok(v) => (200, "application/json", v.to_string()),
                Err(e) => (400, "application/json", err_json(&e)),
            }
        }
        ("GET", "/api/input") => {
            let days = qval("days")
                .and_then(|v| v.parse::<u32>().ok())
                .unwrap_or(7);
            match api_input_at(conn, days, today_naive()) {
                Ok(v) => (200, "application/json", v.to_string()),
                Err(e) => (400, "application/json", err_json(&e)),
            }
        }
        ("GET", "/api/report") => {
            let date = qval("date").unwrap_or_else(|| "today".to_string());
            let s = settings::load(db_path);
            match api_report_at(conn, &date, &s) {
                Ok(v) => (200, "application/json", v.to_string()),
                Err(e) => (400, "application/json", err_json(&e)),
            }
        }
        ("GET", "/api/trends") => (
            200,
            "application/json",
            api_trends_at(conn, today_naive()).to_string(),
        ),
        ("GET", "/api/apps_grid") => {
            let date = qval("date").unwrap_or_else(|| "today".to_string());
            match api_apps_grid_at(conn, &date) {
                Ok(v) => (200, "application/json", v.to_string()),
                Err(e) => (400, "application/json", err_json(&e)),
            }
        }
        ("GET", "/api/daily_top") => {
            let days = qval("days")
                .and_then(|v| v.parse::<u32>().ok())
                .unwrap_or(14);
            (
                200,
                "application/json",
                api_daily_top_at(conn, days, today_naive()).to_string(),
            )
        }
        ("GET", "/api/settings") => (200, "application/json", api_settings(db_path).to_string()),
        ("POST", "/api/settings") => match api_settings_post(db_path, body) {
            Ok(v) => (200, "application/json", v.to_string()),
            Err(e) => (400, "application/json", err_json(&e)),
        },
        ("GET", _) => (404, "application/json", err_json("not found")),
        (_, _) => (405, "application/json", err_json("method not allowed")),
    }
}

/// 今日本地日期（NaiveDate 形，供 heatmap/apps 纯函数注入边界）。
fn today_naive() -> chrono::NaiveDate {
    Local::now().date_naive()
}

fn err_json(msg: &str) -> String {
    json!({"error": msg}).to_string()
}

/// 只读打开：优先走 MCP 工具面同款 `open_reader`（READ_ONLY + 全套 PRAGMA）。
/// 非 WAL 库上 `journal_mode=WAL` 会写入失败，此时回退为纯 READ_ONLY 连接
/// （仅设无副作用的 busy_timeout）——依然零写入、零迁移。READ_ONLY 连接对
/// WAL 库天然兼容（同进程采集器持写连接时多读一写并存）。
fn open_read_only(db_path: &Path) -> Result<Connection> {
    let path = db_path.to_string_lossy().to_string();
    match kynoptic_mcp::state::open_reader(&path) {
        Ok(conn) => Ok(conn),
        Err(_) => {
            let conn =
                Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                    .map_err(|e| {
                        Error::InvalidData(format!("无法只读打开 {}: {e}", db_path.display()))
                    })?;
            conn.execute_batch("PRAGMA busy_timeout=5000;")
                .map_err(|e| Error::InvalidData(e.to_string()))?;
            Ok(conn)
        }
    }
}

// ─── HTTP 服务（std::net 手写最小 handler） ─────────────────────────────────

/// 阻塞服务循环。仅绑定 127.0.0.1；每连接一线程内串行处理、响应后立即关闭。
///
/// `readonly=true`（cli 与 tray 均传 true）：DB 只读打开，唯一写路径是
/// POST /api/settings 写 settings.json；`false` 当前与 true 同策略，保留
/// 参数位以便未来显式放开（本 crate 现无任何 DB 写路径）。
/// 端口 0 = 随机空闲端口（实际端口经 log 输出）。
pub fn serve(db_path: &Path, port: u16, readonly: bool) -> Result<()> {
    let _ = readonly;
    let conn = open_read_only(db_path)?;
    let listener = TcpListener::bind(("127.0.0.1", port)).map_err(|e| {
        Error::Io(std::io::Error::other(format!(
            "绑定 127.0.0.1:{port} 失败: {e}"
        )))
    })?;
    let bound = listener.local_addr().map(|a| a.port()).unwrap_or(port);
    log::info!(
        "dashboard: http://127.0.0.1:{bound}  (db: {}, read-only)",
        db_path.display()
    );
    println!(
        "dashboard: http://127.0.0.1:{bound}  (db: {}, read-only; Ctrl+C 停止)",
        db_path.display()
    );
    for stream in listener.incoming() {
        let Ok(mut stream) = stream else { continue };
        // 连接内共享 &Connection：单线程逐连接串行处理即可（本地面板低并发）。
        let _ = handle_client(&conn, &mut stream, db_path);
    }
    Ok(())
}

/// 读请求行 → route → 写响应。任何失败都静默断开（无日志面需求）。
fn handle_client(conn: &Connection, stream: &mut TcpStream, db_path: &Path) -> std::io::Result<()> {
    // 读请求行 + 头部；POST 再按 Content-Length 补读请求体（上限 64 KiB）。
    let mut buf = [0u8; 4096];
    let mut raw = Vec::new();
    let header_end = loop {
        let n = stream.read(&mut buf)?;
        if n == 0 {
            break raw.len();
        }
        raw.extend_from_slice(&buf[..n]);
        if let Some(pos) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
        if raw.len() > 8192 {
            break raw.len();
        }
    };
    let head = String::from_utf8_lossy(&raw[..header_end.min(raw.len())]).to_string();
    let content_length = head
        .lines()
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.trim()
                .eq_ignore_ascii_case("content-length")
                .then(|| v.trim().parse::<usize>().ok())?
        })
        .unwrap_or(0)
        .min(64 * 1024);
    while raw.len() < header_end + content_length {
        let n = stream.read(&mut buf)?;
        if n == 0 {
            break;
        }
        raw.extend_from_slice(&buf[..n]);
    }
    let all = String::from_utf8_lossy(&raw);
    let request_line = all.lines().next().unwrap_or_default();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let path = parts.next().unwrap_or("/").to_string();
    let body = all.get(header_end..).unwrap_or_default().to_string();

    let (status, ctype, body) = route_req(conn, &method, &path, &body, db_path);
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "Error",
    };
    write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

#[cfg(test)]
mod tests;
