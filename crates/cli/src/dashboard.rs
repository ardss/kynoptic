//! `kynoptic-ctl dashboard` — 本机只读 web 面板
//!
//! 绑定 127.0.0.1 的极简 HTTP 服务（std::net 手写 handler，零新依赖、零 CDN，
//! 静态页 `dashboard.html` 经 `include_str!` 内嵌，完全离线可用）。
//!
//! **只读铁律**：数据库经 `kynoptic_mcp::state::open_reader`（SQLITE_OPEN_READ_ONLY）
//! 打开——不建表、不跑迁移、永不写 events；agg 缓存只经采集器/维护路径写。
//! 面板本身对库零写入，对采集器热路径零影响（独立进程、按请求查库）。
//!
//! 端点（全部 GET、JSON）：
//! - `/api/summary?date=YYYY-MM-DD`  当日 active_minutes / keys / clicks / top_app
//! - `/api/timeline?hours=12`        近 N 小时按本地小时桶的应用分布（top5 + other）
//! - `/api/anomalies?days=7`         复用 MCP `get_anomalies` 的同一异常检测
//! - `/api/status`                   今日日期 + 最新事件时间戳 + db 路径
//! - `/api/settings` (GET)           当前设置 + 全部监控器清单
//! - `/api/settings` (POST)          更新设置（写 settings.json，不触碰 events）
//!
//! 无鉴权：仅绑定回环地址，不暴露到网络（页脚已声明）。

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Local, Utc};
use rusqlite::{params, Connection};
use serde_json::{json, Value};

use kynoptic_core::queries;
use kynoptic_core::registry;
use kynoptic_core::{Error, Result};

use crate::settings::{self, AppSettings};

/// 内嵌静态页（与产品 monospace/终端风一致的暗色单页）。
pub const DASHBOARD_HTML: &str = include_str!("dashboard.html");

const DEFAULT_PORT: u16 = 8422;
const USAGE_DASH: &str = "usage: kynoptic-ctl dashboard [--port N] [--db PATH]\n  --port N   TCP port on 127.0.0.1 (default 8422, 0 = random free port)\n  --db PATH  kynoptic.db path (default: KYNOPTIC_DB > exe-relative > cwd)\n";

// ─── 参数解析 ───────────────────────────────────────────────────────────────

/// 解析 `--port` / `--db`。端口非法或超出范围报错；db 缺省走 core 统一解析。
pub fn parse_args(args: &[String]) -> Result<(u16, PathBuf)> {
    let mut port = DEFAULT_PORT;
    let mut db: Option<PathBuf> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--port" => {
                i += 1;
                let raw = args.get(i).cloned().unwrap_or_default();
                port = raw
                    .parse::<u16>()
                    .map_err(|_| Error::InvalidData(format!("端口非法: {raw}（0-65535）")))?;
            }
            "--db" => {
                i += 1;
                let raw = args.get(i).cloned().unwrap_or_default();
                if raw.is_empty() {
                    return Err(Error::InvalidData("--db 需要路径".into()));
                }
                db = Some(PathBuf::from(raw));
            }
            other => {
                return Err(Error::InvalidData(format!(
                    "未知选项: {other}\n\n{USAGE_DASH}"
                )))
            }
        }
        i += 1;
    }
    Ok((port, db.unwrap_or_else(kynoptic_core::db::resolve_db_path)))
}

// ─── 数据面（&Connection 纯函数，可脱离 TCP 单测） ──────────────────────────

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
        "dashboard_port": s.dashboard_port,
        "monitors": monitors,
    })
}

/// POST /api/settings — 接受 `enabled_monitors` / `autostart` / `dashboard_port`
/// 任一子集；监控器 id 必须全部在注册表内，否则 400。写盘后返回新设置。
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
    if let Some(v) = req.get("autostart") {
        next.autostart = v.as_bool().ok_or("autostart 应为布尔值")?;
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
        ("GET", "/api/settings") => (200, "application/json", api_settings(db_path).to_string()),
        ("POST", "/api/settings") => match api_settings_post(db_path, body) {
            Ok(v) => (200, "application/json", v.to_string()),
            Err(e) => (400, "application/json", err_json(&e)),
        },
        ("GET", _) => (404, "application/json", err_json("not found")),
        (_, _) => (405, "application/json", err_json("method not allowed")),
    }
}

fn err_json(msg: &str) -> String {
    json!({"error": msg}).to_string()
}

/// 只读打开：优先走 MCP 工具面同款 `open_reader`（READ_ONLY + 全套 PRAGMA）。
/// 非 WAL 库上 `journal_mode=WAL` 会写入失败，此时回退为纯 READ_ONLY 连接
/// （仅设无副作用的 busy_timeout）——依然零写入、零迁移。
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

/// 阻塞服务循环。仅绑定 127.0.0.1；每连接一线程、响应后立即关闭。
pub fn serve(port: u16, db_path: &Path) -> Result<()> {
    let conn = open_read_only(db_path)?;
    let listener = TcpListener::bind(("127.0.0.1", port)).map_err(|e| {
        Error::Io(std::io::Error::other(format!(
            "绑定 127.0.0.1:{port} 失败: {e}"
        )))
    })?;
    let bound = listener.local_addr().map(|a| a.port()).unwrap_or(port);
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

/// `kynoptic-ctl dashboard [--port N] [--db PATH]` 入口。
pub fn cmd_dashboard(args: &[String]) -> Result<()> {
    let (port, db_path) = parse_args(args)?;
    if !db_path.exists() {
        return Err(Error::InvalidData(format!(
            "数据库不存在: {}（先用采集器/ctl 生成，dashboard 不建库不迁移）",
            db_path.display()
        )));
    }
    serve(port, &db_path)
}

// ─── 测试（硬件无关：内存库 + 注入时刻） ────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn mem_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(kynoptic_core::db::SCHEMA).unwrap();
        kynoptic_core::db::run_migrations(&conn);
        conn
    }

    /// 插入一条事件。`ts` 为 UTC RFC3339。
    fn insert(conn: &Connection, ts: &str, t: &str, a: &str, app: Option<&str>) {
        conn.execute(
            "INSERT INTO events (timestamp, event_type, event_action, event_data, app_name, window_title, session_id) VALUES (?1,?2,?3,NULL,?4,?4,NULL)",
            params![ts, t, a, app],
        )
        .unwrap();
    }

    /// 本地某日某时的 UTC RFC3339（把本地钟面时刻当真，跨时区稳定）。
    fn local_ts(days_ahead: i64, hour: u32, min: u32) -> String {
        let naive = chrono::NaiveDate::from_ymd_opt(2026, 9, 9)
            .unwrap()
            .and_hms_opt(hour, min, 0)
            .unwrap()
            + chrono::Duration::days(days_ahead);
        use chrono::TimeZone;
        Local
            .from_local_datetime(&naive)
            .earliest()
            .unwrap()
            .with_timezone(&Utc)
            .to_rfc3339()
    }

    // === parse_args ===

    #[test]
    fn dash_args_defaults_and_db_override() {
        let (port, db) = parse_args(&[]).unwrap();
        assert_eq!(port, DEFAULT_PORT);
        assert_eq!(db, kynoptic_core::db::resolve_db_path());
        let (port, db) =
            parse_args(&["--port".into(), "0".into(), "--db".into(), "x/y.db".into()]).unwrap();
        assert_eq!(port, 0);
        assert_eq!(db, PathBuf::from("x/y.db"));
    }

    #[test]
    fn dash_args_rejects_bad_port_and_flag() {
        assert!(parse_args(&["--port".into(), "99999".into()]).is_err());
        assert!(parse_args(&["--port".into(), "abc".into()]).is_err());
        assert!(parse_args(&["--gpu".into()]).is_err());
        assert!(parse_args(&["--db".into()]).is_err());
    }

    // === summary ===

    #[test]
    fn summary_counts_come_from_db() {
        let conn = mem_conn();
        let date = queries::today_local_str();
        let (start, _) = queries::local_day_range(&date).unwrap();
        insert(&conn, &start, "keyboard", "press", None);
        insert(&conn, &start, "keyboard", "press", None);
        insert(&conn, &start, "mouse", "click", None);
        insert(&conn, &start, "window", "switch", Some("code"));
        let v = api_summary(&conn, &date).unwrap();
        assert_eq!(v["keys"], json!(2));
        assert_eq!(v["clicks"], json!(1));
        assert_eq!(v["top_app"], json!("code"));
        assert!(v["active_minutes"].as_i64().unwrap() >= 1);
    }

    #[test]
    fn summary_rejects_bad_date() {
        let conn = mem_conn();
        assert!(api_summary(&conn, "not-a-date").is_err());
    }

    // === timeline 桶化 ===

    #[test]
    fn timeline_buckets_by_local_hour_with_top5_and_other() {
        let conn = mem_conn();
        // 本地 2026-09-09 10:00 与 10:30 各若干事件 + 11:00 一个
        let h10a = local_ts(0, 10, 0);
        let h10b = local_ts(0, 10, 30);
        let h11 = local_ts(0, 11, 0);
        for app in ["a", "a", "b", "c", "d", "e", "f", "g"] {
            insert(&conn, &h10a, "window", "switch", Some(app));
        }
        insert(&conn, &h10b, "keyboard", "press", None);
        insert(&conn, &h11, "window", "switch", Some("z"));
        let now = chrono::DateTime::parse_from_rfc3339(&local_ts(0, 12, 0))
            .unwrap()
            .with_timezone(&Utc);
        let v = api_timeline_at(&conn, 12, now).unwrap();
        let buckets = v["buckets"].as_array().unwrap();
        assert_eq!(buckets.len(), 2, "只含有数据的桶: {v}");
        let b10 = buckets
            .iter()
            .find(|b| b["hour"].as_str().unwrap().ends_with("T10"))
            .unwrap();
        let apps = b10["apps"].as_array().unwrap();
        // 8 个 switch + 1 press = 9 事件，7 个不同应用 → top5 + other
        assert_eq!(apps.len(), 6, "top5 + other: {b10}");
        assert_eq!(apps[5]["app"], json!("(other)"));
        assert_eq!(apps[0]["app"], json!("a"));
        assert_eq!(apps[0]["events"], json!(2));
        // 降序：other(2) 与 a(2) 并列时按名次序，any 顺序均可，但 events 总和守恒
        let total: i64 = apps.iter().map(|x| x["events"].as_i64().unwrap()).sum();
        assert_eq!(total, 9);
    }

    #[test]
    fn timeline_hours_clamped_and_range_excludes_old_events() {
        let conn = mem_conn();
        let now = chrono::DateTime::parse_from_rfc3339(&local_ts(0, 12, 0))
            .unwrap()
            .with_timezone(&Utc);
        // 远超 48h 前的事件不应出现
        let old = (now - chrono::Duration::hours(100)).to_rfc3339();
        insert(&conn, &old, "keyboard", "press", None);
        let v = api_timeline_at(&conn, 9999, now).unwrap();
        assert_eq!(v["hours"], json!(48));
        assert_eq!(v["buckets"].as_array().unwrap().len(), 0);
    }

    // === anomalies（与 MCP 同入口） ===

    #[test]
    fn anomalies_shape_matches_mcp_entry() {
        let conn = mem_conn();
        let v = api_anomalies(&conn, 7);
        assert!(v["anomalies"].is_array());
        assert_eq!(v["truncated"], json!(false));
    }

    // === status ===

    #[test]
    fn status_reflects_last_event_ts() {
        let conn = mem_conn();
        let v = api_status(&conn, Path::new("/tmp/x.db"));
        assert_eq!(v["last_event_ts"], json!(""));
        let ts = local_ts(0, 9, 15);
        insert(&conn, &ts, "keyboard", "press", None);
        let v = api_status(&conn, Path::new("/tmp/x.db"));
        assert_eq!(v["last_event_ts"], json!(queries::latest_event_ts(&conn)));
        assert_eq!(v["today"], json!(queries::today_local_str()));
        assert_eq!(v["read_only"], json!(true));
        assert_eq!(v["bind"], json!("127.0.0.1"));
    }

    // === 路由表 ===

    fn tmpdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "kyn-dash-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn settings_get_returns_registry_and_defaults() {
        let dir = tmpdir("get");
        let db = dir.join("kyn.db");
        let (code, ctype, body) = route_req(&mem_conn(), "GET", "/api/settings", "", &db);
        assert_eq!(code, 200);
        assert_eq!(ctype, "application/json");
        let v: Value = serde_json::from_str(&body).unwrap();
        let monitors = v["monitors"].as_array().unwrap();
        assert_eq!(monitors.len(), 40);
        assert_eq!(
            monitors
                .iter()
                .filter(|m| m["default_enabled"] == json!(true))
                .count(),
            14
        );
        assert_eq!(v["enabled_monitors"].as_array().unwrap().len(), 14);
        assert!(v["autostart"].is_boolean());
        assert!(monitors[0]["sensitivity"].is_string());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn settings_post_updates_and_persists() {
        let dir = tmpdir("post");
        let db = dir.join("kyn.db");
        let body = r#"{"enabled_monitors":["window","keyboard_hook"],"autostart":true,"dashboard_port":9001}"#;
        let (code, _, out) = route_req(&mem_conn(), "POST", "/api/settings", body, &db);
        assert_eq!(code, 200, "{out}");
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["enabled_monitors"], json!(["window", "keyboard_hook"]));
        assert_eq!(v["autostart"], json!(true));
        assert_eq!(v["dashboard_port"], json!(9001));
        // 已写盘：重新 GET 应读到同样的值
        let (_, _, out) = route_req(&mem_conn(), "GET", "/api/settings", "", &db);
        let v2: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v2["enabled_monitors"], json!(["window", "keyboard_hook"]));
        assert_eq!(v2["dashboard_port"], json!(9001));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn settings_post_rejects_unknown_id_and_bad_input() {
        let dir = tmpdir("post-bad");
        let db = dir.join("kyn.db");
        let (code, _, out) = route_req(
            &mem_conn(),
            "POST",
            "/api/settings",
            r#"{"enabled_monitors":["window","not_a_monitor"]}"#,
            &db,
        );
        assert_eq!(code, 400);
        assert!(out.contains("not_a_monitor"));
        // 非法 JSON → 400
        let (code, _, _) = route_req(&mem_conn(), "POST", "/api/settings", "{oops", &db);
        assert_eq!(code, 400);
        // autostart 类型错 → 400
        let (code, _, _) = route_req(
            &mem_conn(),
            "POST",
            "/api/settings",
            r#"{"autostart":"yes"}"#,
            &db,
        );
        assert_eq!(code, 400);
        // 失败后不应留下写坏的设置文件
        let (_, _, out) = route_req(&mem_conn(), "GET", "/api/settings", "", &db);
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["enabled_monitors"].as_array().unwrap().len(), 14);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn route_table_and_error_codes() {
        let conn = mem_conn();
        let db = Path::new("kyn.db");
        let (code, ctype, body) = route_req(&conn, "GET", "/", "", db);
        assert_eq!(code, 200);
        assert!(ctype.starts_with("text/html"));
        assert!(body.contains("All data is local"), "页脚数据声明必须内嵌");

        let (code, ctype, _) = route_req(&conn, "GET", "/api/status", "", db);
        assert_eq!(code, 200);
        assert_eq!(ctype, "application/json");

        let (code, _, body) = route_req(&conn, "GET", "/api/summary?date=bad", "", db);
        assert_eq!(code, 400);
        assert!(body.contains("error"));

        // 缺省 date = 今日 → 200
        let (code, _, _) = route_req(&conn, "GET", "/api/summary", "", db);
        assert_eq!(code, 200);

        let (code, _, _) = route_req(&conn, "GET", "/api/timeline?hours=6", "", db);
        assert_eq!(code, 200);
        let (code, _, _) = route_req(&conn, "GET", "/api/timeline?hours=nope", "", db);
        assert_eq!(code, 200, "hours 非法回退默认 12");

        let (code, _, _) = route_req(&conn, "GET", "/api/anomalies?days=3", "", db);
        assert_eq!(code, 200);

        let (code, _, _) = route_req(&conn, "GET", "/nope", "", db);
        assert_eq!(code, 404);
        let (code, _, _) = route_req(&conn, "POST", "/api/status", "", db);
        assert_eq!(code, 405);
    }

    #[test]
    fn top5_with_other_stable_ordering() {
        let apps: Vec<(String, i64)> = [
            ("a", 1),
            ("b", 9),
            ("c", 9),
            ("d", 3),
            ("e", 2),
            ("f", 1),
            ("g", 1),
        ]
        .iter()
        .map(|(n, c)| ((*n).to_string(), *c))
        .collect();
        let out = top5_with_other(apps);
        assert_eq!(out.len(), 6);
        assert_eq!(out[0], ("b".into(), 9));
        assert_eq!(out[1], ("c".into(), 9));
        assert_eq!(out[5], ("(other)".into(), 2));
        // ≤5 个不合并
        let small: Vec<(String, i64)> = [("x", 1), ("y", 2)]
            .iter()
            .map(|(n, c)| ((*n).to_string(), *c))
            .collect();
        assert_eq!(top5_with_other(small).len(), 2);
    }
}
