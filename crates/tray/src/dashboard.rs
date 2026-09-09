//! 本地只读 dashboard HTTP 服务(tray 内嵌版)。
//!
//! **来源说明**:此模块的 HTTP 服务逻辑(serve / handle_client / route 表)
//! 复制自 `crates/cli/src/dashboard.rs`(该模块是 bin 私有,无法以库形式复用),
//! 仅做了两处适配:去掉 CLI 参数入口、println 换 log;端点语义、只读打开
//! (kynoptic_mcp::state::open_reader)与内嵌静态页(dashboard.html)保持一致。
//! 仍然只绑定 127.0.0.1,零新依赖(std::net 手写 handler),对库零写入。

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;

use chrono::{DateTime, Local, Utc};
use rusqlite::{params, Connection};
use serde_json::{json, Value};

use kynoptic_core::queries;
use kynoptic_core::Error;

/// 内嵌静态页(与 `kynoptic-ctl dashboard` 同一份单页)。
pub const DASHBOARD_HTML: &str = include_str!("../dashboard.html");

/// 阻塞服务循环。仅绑定 127.0.0.1;每连接一线程、响应后立即关闭。
/// 端口 0 = 随机空闲端口(实际端口经 log 输出)。
pub fn serve(port: u16, db_path: &Path) -> Result<(), Error> {
    let conn = open_read_only(db_path)?;
    let listener = TcpListener::bind(("127.0.0.1", port))
        .map_err(|e| Error::InvalidData(format!("绑定 127.0.0.1:{port} 失败: {e}")))?;
    let bound = listener.local_addr().map(|a| a.port()).unwrap_or(port);
    log::info!(
        "dashboard: http://127.0.0.1:{bound}  (db: {}, read-only)",
        db_path.display()
    );
    for stream in listener.incoming() {
        let Ok(mut stream) = stream else { continue };
        // 连接内共享 &Connection:单线程逐连接串行处理即可(本地面板低并发)。
        let _ = handle_client(&conn, &mut stream, db_path);
    }
    Ok(())
}

/// 读请求行 -> route -> 写响应。任何失败都静默断开。
fn handle_client(conn: &Connection, stream: &mut TcpStream, db_path: &Path) -> std::io::Result<()> {
    let mut buf = [0u8; 4096];
    let mut raw = Vec::new();
    loop {
        let n = stream.read(&mut buf)?;
        if n == 0 {
            break;
        }
        raw.extend_from_slice(&buf[..n]);
        if raw.windows(4).any(|w| w == b"\r\n\r\n") || raw.len() > 8192 {
            break;
        }
    }
    let line = String::from_utf8_lossy(&raw);
    let request_line = line.lines().next().unwrap_or_default();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let path = parts.next().unwrap_or("/").to_string();

    let (status, ctype, body) = route(conn, &method, &path, db_path);
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

// ─── 路由(与 cli/dashboard.rs 同表;api_* 均为 &Connection 纯函数) ──────────

fn route(
    conn: &Connection,
    method: &str,
    path: &str,
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
        ("GET", _) => (404, "application/json", err_json("not found")),
        (_, _) => (405, "application/json", err_json("method not allowed")),
    }
}

fn err_json(msg: &str) -> String {
    json!({"error": msg}).to_string()
}

fn api_summary(conn: &Connection, date: &str) -> std::result::Result<Value, String> {
    let (start, end) = queries::local_day_range(date)
        .ok_or_else(|| format!("日期格式错: {date}(应为 YYYY-MM-DD)"))?;
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

fn api_timeline_at(
    conn: &Connection,
    hours: u32,
    now: DateTime<Utc>,
) -> std::result::Result<Value, String> {
    let hours = hours.clamp(1, 48);
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

    let mut by_bucket: std::collections::BTreeMap<String, Vec<(String, i64)>> =
        std::collections::BTreeMap::new();
    for (bucket, app, cnt) in rows {
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

fn local_offset_modifier_at(now: DateTime<Utc>) -> String {
    let secs = now.with_timezone(&Local).offset().local_minus_utc() as i64;
    format!(
        "{}{} seconds",
        if secs >= 0 { "+" } else { "-" },
        secs.abs()
    )
}

fn api_anomalies(conn: &Connection, days: u32) -> Value {
    kynoptic_mcp::state::anomalies(conn, days as usize, 100)
}

fn api_status(conn: &Connection, db_path: &Path) -> Value {
    json!({
        "today": queries::today_local_str(),
        "last_event_ts": queries::latest_event_ts(conn),
        "db_path": db_path.display().to_string(),
        "bind": "127.0.0.1",
        "read_only": true,
    })
}

/// 只读打开(与 cli/dashboard.rs 同策略):优先 MCP open_reader,
/// 非 WAL 库回退纯 READ_ONLY 连接 + busy_timeout。
fn open_read_only(db_path: &Path) -> Result<Connection, Error> {
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
