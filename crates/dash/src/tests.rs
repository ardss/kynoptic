//! 集成面测试：内存库 + 注入时刻/日期，硬件无关。

use super::*;

fn mem_conn() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(kynoptic_core::db::SCHEMA).unwrap();
    let _ = kynoptic_core::db::run_migrations(&conn);
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
    // 补零桶：固定返回窗口内全部 12 个本地小时（无数据小时 human/auto 全 0）
    assert_eq!(buckets.len(), 12, "12 小时窗口全量补零: {v}");
    assert_eq!(buckets[0]["hour"], json!("2026-09-09T00"));
    assert_eq!(buckets[0]["human_min"], json!(0));
    assert_eq!(buckets[0]["auto_min"], json!(0));
    assert_eq!(buckets[0]["apps"].as_array().unwrap().len(), 0);
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
    // 补零桶：即使整窗无数据也固定返回 48 个本地小时，全 0
    let buckets = v["buckets"].as_array().unwrap();
    assert_eq!(buckets.len(), 48, "补零后恒为 48 桶: {v}");
    assert!(buckets
        .iter()
        .all(|b| b["human_min"] == json!(0) && b["auto_min"] == json!(0)));
    // DST 口径说明字段
    assert!(v["local_offset_seconds"].is_number());
    assert!(v["local_offset_note"].as_str().unwrap().contains("DST"));
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
    // 审查 P2：db_path 只回文件名，不暴露全路径
    assert_eq!(v["db_path"], json!("x.db"));
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
fn heatmap_fills_missing_days_with_zero_and_counts_input_minutes() {
    let conn = mem_conn();
    // 口径（审查 DeepSeek）：热力图 = 每日"有键鼠输入的分钟数"（agg_minute，
    // 绝对值），不再是事件条数。今天 2 个输入分钟，前天 1 个。
    for (h, m) in [(10, 0), (10, 30)] {
        conn.execute(
            "INSERT INTO agg_minute (date, hour, minute, bucket_id, sum_value, count_value) VALUES ('2026-09-09', ?1, ?2, 'input_keys', 5, 5)",
            rusqlite::params![h, m],
        )
        .unwrap();
    }
    conn.execute(
        "INSERT INTO agg_minute (date, hour, minute, bucket_id, sum_value, count_value) VALUES ('2026-09-07', 9, 15, 'input_keys', 3, 3)",
        [],
    )
    .unwrap();
    let today = chrono::NaiveDate::from_ymd_opt(2026, 9, 9).unwrap();
    let v = api_heatmap_at(&conn, 1, today);
    let days = v["days"].as_array().unwrap();
    assert_eq!(days.len(), 7, "weeks=1 → 7 天补全: {v}");
    assert_eq!(days[6]["date"], json!("2026-09-09"));
    assert_eq!(days[6]["value"], json!(2), "两个不同输入分钟");
    assert_eq!(days[4]["date"], json!("2026-09-07"));
    assert_eq!(days[4]["value"], json!(1));
    assert_eq!(days[0]["value"], json!(0), "缺数天补零");
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
    // 本地数据完整优先：每键频次采集默认开启
    assert_eq!(v["vk_frequency_enabled"], json!(true));
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

#[test]
fn settings_post_vk_frequency_bool_and_audit_log() {
    let dir = tmpdir("vk-audit");
    let db = dir.join("kyn.db");
    let audit = dir.join("settings-audit.log");
    // vk_frequency_enabled 类型错 → 400
    let (code, _, _) = route_req(
        &mem_conn(),
        "POST",
        "/api/settings",
        r#"{"vk_frequency_enabled":"on"}"#,
        &db,
    );
    assert_eq!(code, 400);
    // 实际变更（默认 true → 显式 opt-out false）→ 200 + GET 回读 false + 审计行
    // （旧→新，无 categories 全文）
    let (code, _, out) = route_req(
        &mem_conn(),
        "POST",
        "/api/settings",
        r#"{"vk_frequency_enabled":false}"#,
        &db,
    );
    assert_eq!(code, 200, "{out}");
    let (_, _, out) = route_req(&mem_conn(), "GET", "/api/settings", "", &db);
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["vk_frequency_enabled"], json!(false));
    let log = std::fs::read_to_string(&audit).unwrap();
    assert_eq!(log.lines().count(), 1, "一次实际变更一行审计: {log}");
    let line = log.lines().next().unwrap();
    assert!(line.contains("vk_frequency_enabled"), "{log}");
    assert!(line.contains("[true,false]"), "旧值→新值: {log}");
    assert!(!line.contains("pattern"), "不得落 categories 全文: {log}");
    assert!(line.len() <= 512 + 1, "单行截断 512 字节: {log}");
    // RFC3339 时间戳前缀
    let ts = line.split('\t').next().unwrap();
    assert!(chrono::DateTime::parse_from_rfc3339(ts).is_ok(), "{log}");
    // 无实际变更的 POST 不追加审计行
    let (code, _, _) = route_req(
        &mem_conn(),
        "POST",
        "/api/settings",
        r#"{"vk_frequency_enabled":false}"#,
        &db,
    );
    assert_eq!(code, 200);
    let log = std::fs::read_to_string(&audit).unwrap();
    assert_eq!(log.lines().count(), 1, "无变更不写审计: {log}");
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

#[test]
fn loopback_host_whitelist() {
    use super::loopback_host_ok;
    assert!(loopback_host_ok("127.0.0.1:8422", 8422));
    assert!(loopback_host_ok("127.0.0.1", 8422));
    assert!(loopback_host_ok("localhost", 8422));
    assert!(loopback_host_ok("localhost:8422", 8422));
    assert!(loopback_host_ok("127.0.0.1:8422.", 8422));
    // DNS rebinding：攻击者域名 rebind 到 127.0.0.1，Host 是攻击者域 → 拒绝
    assert!(!loopback_host_ok("evil.example.com", 8422));
    assert!(!loopback_host_ok("", 8422));
    assert!(!loopback_host_ok("192.168.1.5:8422", 8422));
    assert!(!loopback_host_ok("127.0.0.2:8422", 8422));
}

// === 三指标口径统一（mixed_minutes / unattended_fg） ===

fn insert_input_agg(conn: &Connection, ts: &str, data: &str) {
    conn.execute(
        "INSERT INTO events (timestamp, event_type, event_action, event_data) VALUES (?1, 'keyboard', 'input_agg', ?2)",
        params![ts, data],
    )
    .unwrap();
}

#[test]
fn overview_counts_mixed_minutes_for_both_presence_and_automation() {
    let conn = mem_conn();
    let today = queries::today_local_str();
    let (start, _) = queries::local_day_range(&today).unwrap();
    let t0 = chrono::DateTime::parse_from_rfc3339(&start).unwrap();
    let t1 = (t0 + chrono::Duration::minutes(1)).to_rfc3339();
    let t2 = (t0 + chrono::Duration::minutes(2)).to_rfc3339();
    // 纯人分钟 / 混合分钟 / 纯自动化分钟
    insert_input_agg(&conn, &start, r#"{"keys": 10}"#);
    insert_input_agg(&conn, &t1, r#"{"keys": 10, "injected_keys": 5}"#);
    insert_input_agg(&conn, &t2, r#"{"injected_keys": 7}"#);
    let v = api_overview(&conn, Path::new("definitely-missing.db"));
    // 混合分钟同时计入两者：presence=2（m0+m1），automation=2（m1+m2），mixed=1
    assert_eq!(v["presence_minutes"], json!(2), "{v}");
    assert_eq!(v["automation_minutes"], json!(2), "{v}");
    assert_eq!(v["mixed_minutes"], json!(1), "{v}");
    assert!(v["unattended_fg_minutes"].is_number());
    assert!(v["metrics_note"].as_str().unwrap().contains("mixed"));
}

// === insights：节律卡（首末输入 + 单事件日标注） ===

#[test]
fn insights_rhythm_updates_last_and_marks_single_event_day() {
    let conn = mem_conn();
    // 昨天两批输入（首 09:00 / 末 17:00），今天仅一条
    for _ in 0..51 {
        insert(&conn, &local_ts(-1, 9, 0), "keyboard", "press", None);
    }
    for _ in 0..3 {
        insert(&conn, &local_ts(-1, 17, 0), "keyboard", "press", None);
    }
    insert(&conn, &local_ts(0, 22, 30), "keyboard", "press", None);
    let v = api_insights(&conn, 2);
    let rhythm = v["insights"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["title_en"] == json!("Your daily rhythm"))
        .expect("节律卡应存在");
    let zh = rhythm["text_zh"].as_str().unwrap();
    let en = rhythm["text_en"].as_str().unwrap();
    assert!(zh.contains("17:00"), "最后输入应被更新而非停在首条: {zh}");
    assert!(zh.contains("仅一条输入记录"), "单事件日应标注: {zh}");
    assert!(en.contains("single input event"), "en 侧同步: {en}");
}

// === anomalies：message_en 映射 ===

#[test]
fn anomalies_message_en_covers_all_kinds() {
    assert!(anomaly_message_en("late_night").contains("Late-night"));
    assert!(anomaly_message_en("apm_burst").contains("APM"));
    assert!(anomaly_message_en("marathon").contains("Marathon"));
    assert!(anomaly_message_en("new_app_surge").contains("surge"));
    assert!(anomaly_message_en("unknown_kind").contains("unknown_kind"));
}

// === trends：presence_minutes 别名 ===

#[test]
fn trends_presence_minutes_alias_matches_active_minutes() {
    let conn = mem_conn();
    conn.execute(
        "INSERT INTO daily_agg (date, keys, clicks, active_minutes) VALUES ('2026-09-08', 10, 2, 45)",
        [],
    )
    .unwrap();
    let today = chrono::NaiveDate::from_ymd_opt(2026, 9, 9).unwrap();
    let v = api_trends_at(&conn, today);
    let d = &v["daily"].as_array().unwrap()[0];
    assert_eq!(d["active_minutes"], json!(45));
    assert_eq!(d["presence_minutes"], json!(45), "别名同值: {d}");
    assert!(v["presence_minutes_note"].as_str().unwrap().contains("alias"));
}

// === 真 socket 测试（审查清单 A5）：真实 TcpListener + 真实 TCP 连接 ===
//
// route_req 之上的网络层行为：并发、8KB+ 头 431、缺 Host 4xx、垃圾字节
// 断连不 panic。serve 阻塞运行在临时线程，端口用「先 bind :0 探测空闲再
// 交给 serve」的方式取得（窗口极小，重试连接兜底）。

mod socket_tests {
    use super::super::serve;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::path::PathBuf;
    use std::time::Duration;

    /// 建一个已初始化的临时 DB 文件，返回路径（serve 只读打开它）
    fn temp_db(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "kyn-dash-sock-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("kyn.db");
        let _db = kynoptic_core::db::Database::open(db.to_str().unwrap()).unwrap();
        drop(_db); // 归还全部连接后再交给 serve
        db
    }

    fn free_port() -> u16 {
        let l = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        l.local_addr().unwrap().port()
    }

    /// 起真实 serve 线程，返回 (端口, 临时目录)
    fn start_server(tag: &str) -> (u16, PathBuf) {
        let db = temp_db(tag);
        let dir = db.parent().unwrap().to_path_buf();
        let port = free_port();
        let db_for_thread = db.clone();
        std::thread::Builder::new()
            .name("e2e-serve".into())
            .spawn(move || {
                let _ = serve(&db_for_thread, port, true);
            })
            .unwrap();
        // 重试连接直到监听就绪（最多 ~5s）
        let mut last_err = None;
        for _ in 0..100 {
            match TcpStream::connect(("127.0.0.1", port)) {
                Ok(s) => {
                    drop(s);
                    return (port, dir);
                }
                Err(e) => last_err = Some(e),
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("serve 未在 5s 内开始监听 {port}: {last_err:?}");
    }

    fn get(port: u16, raw_request: &str) -> String {
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        s.set_write_timeout(Some(Duration::from_secs(5))).unwrap();
        s.write_all(raw_request.as_bytes()).unwrap();
        let mut buf = Vec::new();
        let _ = s.read_to_end(&mut buf); // Connection: close → 读到 EOF
        String::from_utf8_lossy(&buf).to_string()
    }

    #[test]
    fn twenty_concurrent_connections_all_get_200() {
        let (port, dir) = start_server("conc");
        let mut handles = Vec::new();
        for _ in 0..20 {
            handles.push(std::thread::spawn(move || {
                let resp = get(port, "GET /api/status HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n");
                resp.starts_with("HTTP/1.1 200 ")
            }));
        }
        let all_ok = handles.into_iter().all(|h| h.join().unwrap());
        assert!(all_ok, "20 个并发连接必须全部得到 200");
        // serve 线程仍持只读连接，Windows 上目录删除是尽力而为
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn oversized_headers_get_431() {
        let (port, dir) = start_server("big");
        // >8KiB 且不带头终止符 → 431 Request Header Fields Too Large
        let big = format!(
            "GET / HTTP/1.1\r\nHost: 127.0.0.1\r\nX-Big: {}\r\n",
            "A".repeat(9000)
        );
        let resp = get(port, &big);
        assert!(
            resp.starts_with("HTTP/1.1 431 "),
            "8KB+ 头应返回 431，实际: {resp}"
        );
        // 服务必须存活：随后正常请求仍 200
        let resp = get(port, "GET /api/status HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n");
        assert!(resp.starts_with("HTTP/1.1 200 "));
        // serve 线程仍持只读连接，Windows 上目录删除是尽力而为
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_host_header_rejected_with_4xx() {
        let (port, dir) = start_server("nohost");
        let resp = get(port, "GET /api/status HTTP/1.1\r\n\r\n");
        let code: u16 = resp
            .split_whitespace()
            .nth(1)
            .and_then(|c| c.parse().ok())
            .unwrap_or(0);
        assert!(
            (400..500).contains(&code),
            "缺 Host（DNS rebinding 防线）必须 4xx，实际: {resp}"
        );
        // serve 线程仍持只读连接，Windows 上目录删除是尽力而为
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn garbage_bytes_close_connection_without_killing_server() {
        let (port, dir) = start_server("garbage");
        // 垃圾字节：连接应被关闭（EOF/错误），服务不得 panic
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        s.write_all(&[0xde, 0xad, 0xbe, 0xef, 0x00, 0xff, 0x13, 0x37])
            .unwrap();
        let mut buf = [0u8; 256];
        let _ = s.read(&mut buf); // EOF 或错误都算"连接被关闭"
        drop(s);
        // 半个请求后挂断：同样不应影响后续连接
        let mut s2 = TcpStream::connect(("127.0.0.1", port)).unwrap();
        s2.write_all(b"GET / HT").unwrap();
        drop(s2);
        // 服务存活：正常请求仍 200
        let resp = get(port, "GET /api/status HTTP/1.1\r\nHost: localhost\r\n\r\n");
        assert!(
            resp.starts_with("HTTP/1.1 200 "),
            "垃圾字节后服务必须存活: {resp}"
        );
        // serve 线程仍持只读连接，Windows 上目录删除是尽力而为
        let _ = std::fs::remove_dir_all(&dir);
    }
}
