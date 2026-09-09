//! 集成面测试：内存库 + 注入时刻/日期，硬件无关。

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
    // events 总和守恒
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

// === overview ===

#[test]
fn overview_reports_session_today_events_and_uptime() {
    let conn = mem_conn();
    // 进行中 session：本地今日 08:00 开始
    let started = local_ts(0, 8, 0);
    conn.execute(
        "INSERT INTO sessions (start_time, end_time, total_events, idle_seconds) VALUES (?1, NULL, 0, 0)",
        params![&started],
    )
    .unwrap();
    let today = queries::today_local_str();
    let (start, _) = queries::local_day_range(&today).unwrap();
    insert(&conn, &start, "keyboard", "press", None);
    insert(&conn, &start, "window", "switch", Some("code"));

    let v = api_overview(&conn, Path::new("definitely-missing.db"));
    assert_eq!(v["today_events"], json!(2));
    assert_eq!(v["session_started_at"], json!(started));
    assert_eq!(v["session_open"], json!(true));
    let uptime = v["uptime_seconds"].as_i64().unwrap();
    assert!(uptime > 0, "进行中 session uptime 应为正: {v}");
    assert_eq!(v["monitors_enabled"], json!(14));
    assert_eq!(v["monitors_total"], json!(40));
    assert!(v["db_size_bytes"].is_number());
    assert!(v["db_wal_bytes"].is_number());
    // 无 system/heartbeat 事件 → cpu/mem 为 null，不报错
    assert!(v["cpu_pct"].is_null());
    assert!(v["mem_pct"].is_null());
    assert_eq!(v["foreground_app"], json!("code"));
}

#[test]
fn overview_closed_session_and_empty_db() {
    let conn = mem_conn();
    let started = local_ts(-1, 8, 0);
    let ended = local_ts(-1, 9, 0);
    conn.execute(
        "INSERT INTO sessions (start_time, end_time, total_events, idle_seconds) VALUES (?1, ?2, 5, 0)",
        params![&started, &ended],
    )
    .unwrap();
    let v = api_overview(&conn, Path::new("definitely-missing.db"));
    assert_eq!(v["session_open"], json!(false));
    assert_eq!(v["today_events"], json!(0));
}

#[test]
fn overview_cpu_mem_from_system_heartbeat_like_current_status() {
    let conn = mem_conn();
    // 与 mcp state 测试同构的 heartbeat 事件（current_state 表缺省路径）
    let ts = local_ts(0, 10, 0);
    conn.execute(
        "INSERT INTO events (timestamp, event_type, event_action, event_data) VALUES (?1, 'system', 'heartbeat', ?2)",
        params![&ts, r#"{"cpu_percent": 42.5, "memory": {"used_percent": 61.0}}"#],
    )
    .unwrap();
    let v = api_overview(&conn, Path::new("definitely-missing.db"));
    assert_eq!(v["cpu_pct"], json!(42.5));
    assert_eq!(v["mem_pct"], json!(61.0));
}

// === heatmap ===

#[test]
fn heatmap_fills_missing_days_with_zero_and_counts_events() {
    let conn = mem_conn();
    // 本地 2026-09-09（今天，测试锚点）3 事件，09-07 1 事件
    for _ in 0..3 {
        insert(&conn, &local_ts(0, 9, 0), "keyboard", "press", None);
    }
    insert(&conn, &local_ts(-2, 9, 0), "mouse", "click", None);
    let today = chrono::NaiveDate::from_ymd_opt(2026, 9, 9).unwrap();
    let v = api_heatmap_at(&conn, 1, today);
    let days = v["days"].as_array().unwrap();
    assert_eq!(days.len(), 7, "weeks=1 → 7 天补全: {v}");
    assert_eq!(days[6]["date"], json!("2026-09-09"));
    assert_eq!(days[6]["value"], json!(3));
    assert_eq!(days[4]["date"], json!("2026-09-07"));
    assert_eq!(days[4]["value"], json!(1));
    assert_eq!(days[0]["value"], json!(0), "缺数天补零");
    // 未来不出现（今日为界）
    assert_eq!(days[0]["date"], json!("2026-09-03"));
}

#[test]
fn heatmap_weeks_clamped() {
    let conn = mem_conn();
    let today = chrono::NaiveDate::from_ymd_opt(2026, 9, 9).unwrap();
    let v = api_heatmap_at(&conn, 999, today);
    assert_eq!(v["weeks"], json!(52));
    assert_eq!(v["days"].as_array().unwrap().len(), 52 * 7);
}

// === apps ===

#[test]
fn apps_ranks_window_events_and_excludes_empty_names() {
    let conn = mem_conn();
    let t = local_ts(0, 9, 0);
    for _ in 0..3 {
        insert(&conn, &t, "window", "switch", Some("code"));
    }
    for _ in 0..2 {
        insert(&conn, &t, "window", "switch", Some("web"));
    }
    // 空名（app_name 空 + window_title 空）必须排除
    insert(&conn, &t, "window", "switch", Some(""));
    // keyboard 事件不计入（仅 window 类）
    insert(&conn, &t, "keyboard", "press", None);
    // 8 天前的事件不在 7 天窗口内
    insert(&conn, &local_ts(-8, 9, 0), "window", "switch", Some("old"));
    let today = chrono::NaiveDate::from_ymd_opt(2026, 9, 9).unwrap();
    let v = api_apps_at(&conn, 7, today);
    let apps = v["apps"].as_array().unwrap();
    assert_eq!(apps.len(), 2, "空名与窗口外排除: {v}");
    assert_eq!(apps[0]["app"], json!("code"));
    assert_eq!(apps[0]["count"], json!(3));
    assert_eq!(apps[1]["app"], json!("web"));
    assert_eq!(apps[1]["count"], json!(2));
}

// === hours ===

#[test]
fn hours_fills_24_buckets_with_zeros() {
    let conn = mem_conn();
    insert(&conn, &local_ts(0, 9, 0), "keyboard", "press", None);
    insert(&conn, &local_ts(0, 9, 30), "window", "switch", Some("code"));
    insert(&conn, &local_ts(0, 22, 0), "mouse", "click", None);
    let v = api_hours(&conn, "2026-09-09").unwrap();
    assert_eq!(v["date"], json!("2026-09-09"));
    let values = v["values"].as_array().unwrap();
    assert_eq!(values.len(), 24);
    assert_eq!(values[9], json!(2));
    assert_eq!(values[22], json!(1));
    assert_eq!(values[8], json!(0), "缺时补零");
}

#[test]
fn hours_today_alias_and_bad_date() {
    let conn = mem_conn();
    let v = api_hours(&conn, "today").unwrap();
    assert_eq!(v["date"], json!(queries::today_local_str()));
    assert!(api_hours(&conn, "not-a-date").is_err());
}

// === settings ===

fn tmpdir(tag: &str) -> std::path::PathBuf {
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
    let body = r#"{"enabled_monitors":["window","keyboard_hook"],"autostart":true,"dashboard_port":9001,"input_counts_only":false}"#;
    let (code, _, out) = route_req(&mem_conn(), "POST", "/api/settings", body, &db);
    assert_eq!(code, 200, "{out}");
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["enabled_monitors"], json!(["window", "keyboard_hook"]));
    assert_eq!(v["autostart"], json!(true));
    assert_eq!(v["dashboard_port"], json!(9001));
    assert_eq!(v["input_counts_only"], json!(false));
    // 已写盘：重新 GET 应读到同样的值
    let (_, _, out) = route_req(&mem_conn(), "GET", "/api/settings", "", &db);
    let v2: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v2["enabled_monitors"], json!(["window", "keyboard_hook"]));
    assert_eq!(v2["dashboard_port"], json!(9001));
    assert_eq!(v2["input_counts_only"], json!(false));
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
    // input_counts_only 类型错 → 400
    let (code, _, _) = route_req(
        &mem_conn(),
        "POST",
        "/api/settings",
        r#"{"input_counts_only":1}"#,
        &db,
    );
    assert_eq!(code, 400);
    // 失败后不应留下写坏的设置文件
    let (_, _, out) = route_req(&mem_conn(), "GET", "/api/settings", "", &db);
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["enabled_monitors"].as_array().unwrap().len(), 14);
    std::fs::remove_dir_all(&dir).unwrap();
}

// === 路由表 ===

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

    let (code, _, body) = route_req(&conn, "GET", "/api/overview", "", db);
    assert_eq!(code, 200);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert!(v["today_events"].is_number());
    assert!(v["db_size_bytes"].is_number());

    let (code, _, body) = route_req(&conn, "GET", "/api/heatmap?weeks=12", "", db);
    assert_eq!(code, 200);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["days"].as_array().unwrap().len(), 84);

    let (code, _, body) = route_req(&conn, "GET", "/api/apps?days=7", "", db);
    assert_eq!(code, 200);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert!(v["apps"].is_array());

    let (code, _, _) = route_req(&conn, "GET", "/api/hours?date=today", "", db);
    assert_eq!(code, 200);
    let (code, _, _) = route_req(&conn, "GET", "/api/hours?date=nope", "", db);
    assert_eq!(code, 400);

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
