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
    let v = api_timeline_at(&conn, 12, now, 2).unwrap();
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
    let v = api_timeline_at(&conn, 9999, now, 2).unwrap();
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

// === anomalies（与 MCP 同入口；marathon 桥接阈值读 settings） ===

#[test]
fn anomalies_shape_matches_core_entry_with_bridge() {
    let conn = mem_conn();
    // settings 文件不存在 → 默认值（bridge=2），不影响 shape
    let db = Path::new("/tmp/kynoptic-test-nonexistent/settings.json");
    let v = api_anomalies(&conn, 7, db);
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
    // 审查 P2：db_path 只回文件名，不暴露全路径；db_dir_kind 只给目录类别
    assert_eq!(v["db_path"], json!("x.db"));
    assert!(
        matches!(
            v["db_dir_kind"].as_str(),
            Some("exe-relative data") | Some("user data")
        ),
        "db_dir_kind 应为类别提示而非路径: {}",
        v["db_dir_kind"]
    );
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

/// has_full_day：数据起点早于今天才 true；仅今日有数据（首装第一小时）
/// 为 false，前端据此在"机器值班"卡显示空态而非误导数字。
#[test]
fn overview_has_full_day_gates_unattended_metric() {
    // 仅今日有数据 → false
    let conn = mem_conn();
    let today = queries::today_local_str();
    let (start, _) = queries::local_day_range(&today).unwrap();
    insert(&conn, &start, "keyboard", "press", None);
    let v = api_overview(&conn, Path::new("definitely-missing.db"));
    assert_eq!(v["has_full_day"], json!(false));

    // 有昨日数据 → true（用真实"昨天"，不依赖固定锚定日期）
    let conn2 = mem_conn();
    let (ystart, _) = queries::local_day_range(&queries::date_offset_str(-1)).unwrap();
    insert(&conn2, &ystart, "keyboard", "press", None);
    let v2 = api_overview(&conn2, Path::new("definitely-missing.db"));
    assert_eq!(v2["has_full_day"], json!(true));

    // 空库（MIN 为 NULL）→ false
    let conn3 = mem_conn();
    let v3 = api_overview(&conn3, Path::new("definitely-missing.db"));
    assert_eq!(v3["has_full_day"], json!(false));
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
    // 口径（审查 DeepSeek）：热力图 = 每日"有键鼠输入的分钟数"（daily_agg
    // 派生缓存，绝对值），不再是事件条数。今天 2 个输入分钟，前天 1 个。
    conn.execute(
        "INSERT INTO daily_agg (date, keys, clicks, active_minutes) VALUES ('2026-09-09', 10, 2, 2)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO daily_agg (date, keys, clicks, active_minutes) VALUES ('2026-09-07', 3, 1, 1)",
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

// 性能实证修复的等价性测试：heatmap 改读 daily_agg（PK 直取）后，
// 输出值必须与旧 agg_minute 去重分钟算法在同数据上一致。
#[test]
fn heatmap_daily_agg_equivalent_to_agg_minute_distinct_minutes() {
    let conn = mem_conn();
    // 同一天同时造两种口径的数据：agg_minute 3 个不同输入分钟；
    // daily_agg.active_minutes 记 3（与去重分钟同源，采集器维护）。
    for (h, m) in [(10, 0), (10, 30), (11, 15)] {
        conn.execute(
            "INSERT INTO agg_minute (date, hour, minute, bucket_id, sum_value, count_value) VALUES ('2026-09-09', ?1, ?2, 'input_keys', 5, 5)",
            rusqlite::params![h, m],
        )
        .unwrap();
    }
    conn.execute(
        "INSERT INTO daily_agg (date, keys, clicks, active_minutes) VALUES ('2026-09-09', 30, 5, 3)",
        [],
    )
    .unwrap();
    // 旧算法参考值：agg_minute GROUP BY date 的去重分钟
    let legacy: i64 = conn
        .query_row(
            "SELECT COUNT(DISTINCT hour * 60 + minute) FROM agg_minute WHERE sum_value > 0 AND bucket_id IN ('input_keys','input_clicks','input_moves') AND date = '2026-09-09'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let today = chrono::NaiveDate::from_ymd_opt(2026, 9, 9).unwrap();
    let v = api_heatmap_at(&conn, 1, today);
    let value = v["days"].as_array().unwrap()[6]["value"].as_i64().unwrap();
    assert_eq!(legacy, 3);
    assert_eq!(
        value, legacy,
        "daily_agg 口径必须与 agg_minute 去重分钟等价: {v}"
    );
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

#[test]
fn audit_oversized_record_becomes_placeholder_not_half_json() {
    // 审查 P2：超限记录整条丢弃并写占位 JSON，不得在 512 字节处把记录劈成两半
    let dir = tmpdir("audit-trunc");
    let db = dir.join("kyn.db");
    let audit = dir.join("settings-audit.log");
    // categories 100 条 × 256 字节 name → 摘要只记条数，仍很小；改用超长路径
    // 无从下手——直接构造大变更：100 条 categories 各 256 字节规则名不会进
    // 摘要（只记条数），所以改从 append_settings_audit 层注入超大 summary。
    let huge_summary = format!("\"x\":\"{}\"", "y".repeat(2048));
    append_settings_audit(&db, &huge_summary);
    // 再写一条正常记录，确认日志行序与完整性不受影响
    append_settings_audit(&db, r#"{"autostart":[false,true]}"#);
    let log = std::fs::read_to_string(&audit).unwrap();
    let lines: Vec<&str> = log.lines().collect();
    assert_eq!(lines.len(), 2, "{log}");
    let v: Value = serde_json::from_str(lines[0].split_once('\t').unwrap().1)
        .expect("占位记录必须是完整 JSON");
    assert_eq!(v["truncated_record"], json!(true));
    assert!(v["len"].as_u64().unwrap() > 512, "{log}");
    let v2: Value =
        serde_json::from_str(lines[1].split_once('\t').unwrap().1).expect("正常记录完整 JSON");
    assert_eq!(v2["autostart"], json!([false, true]));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn date_echo_is_sanitized_in_errors() {
    // 审查 P2：400 回显只留字母数字与 '-'，截 32 字符
    assert_eq!(sanitize_date_echo("2026-01-02"), "2026-01-02");
    // 过滤掉了字符 → 以省略号标注"被净化"
    assert_eq!(
        sanitize_date_echo("<script>alert(1)</script>"),
        "scriptalert1script…"
    );
    let long = "a".repeat(64);
    let out = sanitize_date_echo(&long);
    assert!(out.chars().count() <= 33, "{out}"); // 32 + 省略号
    assert!(out.ends_with('…'));
    let (code, _, body) = route_req(
        &mem_conn(),
        "GET",
        "/api/summary?date=<script>alert(1)</script>",
        "",
        std::path::Path::new("kyn.db"),
    );
    assert_eq!(code, 400);
    assert!(!body.contains("<script>"), "原始输入不得原样回显: {body}");
}

// === fuzz 加固：设置面输入校验 ===

#[test]
fn settings_post_rejects_port_zero_and_out_of_range() {
    let dir = tmpdir("port-zero");
    let db = dir.join("kyn.db");
    // 0 是毒值：落库后面板绑定不到有效端口
    for port in [0u64, 65536, 99999] {
        let (code, _, out) = route_req(
            &mem_conn(),
            "POST",
            "/api/settings",
            &format!(r#"{{"dashboard_port":{port}}}"#),
            &db,
        );
        assert_eq!(code, 400, "port {port} 必须被拒绝: {out}");
    }
    // 负数/字符串类型错同样 400
    let (code, _, _) = route_req(
        &mem_conn(),
        "POST",
        "/api/settings",
        r#"{"dashboard_port":-1}"#,
        &db,
    );
    assert_eq!(code, 400);
    // 有效值 1 与 65535 仍被接受
    for port in [1u64, 65535] {
        let (code, _, out) = route_req(
            &mem_conn(),
            "POST",
            "/api/settings",
            &format!(r#"{{"dashboard_port":{port}}}"#),
            &db,
        );
        assert_eq!(code, 200, "port {port} 应有效: {out}");
    }
    // 毒值未落库
    let (_, _, out) = route_req(&mem_conn(), "GET", "/api/settings", "", &db);
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["dashboard_port"], json!(65535));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn settings_post_dedups_enabled_monitors() {
    let dir = tmpdir("dedup");
    let db = dir.join("kyn.db");
    // fuzz 实证：500 元素含 495 重复照落。现在应去重保序，只留唯一 id
    let ids: Vec<String> = ["window", "keyboard_hook", "mouse_hook"]
        .iter()
        .flat_map(|id| std::iter::repeat_n(id.to_string(), 170))
        .collect();
    let body = format!(r#"{{"enabled_monitors":{}}}"#, json!(ids));
    let (code, _, out) = route_req(&mem_conn(), "POST", "/api/settings", &body, &db);
    assert_eq!(code, 200, "{out}");
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(
        v["enabled_monitors"],
        json!(["window", "keyboard_hook", "mouse_hook"]),
        "重复 id 应去重且保序: {out}"
    );
    // 已写盘
    let (_, _, out) = route_req(&mem_conn(), "GET", "/api/settings", "", &db);
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(
        v["enabled_monitors"],
        json!(["window", "keyboard_hook", "mouse_hook"])
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn settings_post_rejects_oversized_categories() {
    let dir = tmpdir("cat-limit");
    let db = dir.join("kyn.db");
    let mk = |n: usize, name_len: usize| {
        let rules: Vec<Value> = (0..n)
            .map(|i| {
                json!({
                    "name": "x".repeat(if i == 0 { name_len } else { 1 }),
                    "pattern": format!("p{i}"),
                })
            })
            .collect();
        json!(rules).to_string()
    };
    let wrap = |arr: String| format!(r#"{{"categories":{arr}}}"#);
    // 101 条 → 400
    let (code, _, out) = route_req(&mem_conn(), "POST", "/api/settings", &wrap(mk(101, 1)), &db);
    assert_eq!(code, 400, "101 条必须被拒绝: {out}");
    assert!(out.contains("100"), "报错应可读（含上限 100）: {out}");
    // 100 条但首条 name 257 字节 → 400
    let (code, _, out) = route_req(
        &mem_conn(),
        "POST",
        "/api/settings",
        &wrap(mk(100, 257)),
        &db,
    );
    assert_eq!(code, 400, "超长 name 必须被拒绝: {out}");
    assert!(out.contains("256"), "报错应可读（含上限 256）: {out}");
    // pattern 超长 → 400
    let body = json!({"categories":[{"name": "ok", "pattern": "y".repeat(257)}]}).to_string();
    let (code, _, _) = route_req(&mem_conn(), "POST", "/api/settings", &body, &db);
    assert_eq!(code, 400);
    // 边界内（100 条 + 256 字节 name）→ 200
    let (code, _, out) = route_req(
        &mem_conn(),
        "POST",
        "/api/settings",
        &wrap(mk(100, 256)),
        &db,
    );
    assert_eq!(code, 200, "100 条 + 256 字节应可接受: {out}");
    // 毒值未落库
    let (_, _, out) = route_req(&mem_conn(), "GET", "/api/settings", "", &db);
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["categories"].as_array().unwrap().len(), 100);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn settings_post_reports_ignored_unknown_fields() {
    let dir = tmpdir("ignored");
    let db = dir.join("kyn.db");
    let (code, _, out) = route_req(
        &mem_conn(),
        "POST",
        "/api/settings",
        r#"{"autostart":true,"hack_admin":true,"mystery":[1]}"#,
        &db,
    );
    assert_eq!(code, 200, "未知字段不 400（patch 兼容）: {out}");
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(
        v["ignored"],
        json!(["hack_admin", "mystery"]),
        "未知顶层字段应在响应 ignored 列表: {out}"
    );
    // 无未知字段时不带 ignored 键
    let (code, _, out) = route_req(
        &mem_conn(),
        "POST",
        "/api/settings",
        r#"{"autostart":false}"#,
        &db,
    );
    assert_eq!(code, 200);
    let v: Value = serde_json::from_str(&out).unwrap();
    assert!(
        v.get("ignored").is_none(),
        "无未知字段不应有 ignored: {out}"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

// === settings：并发保存风暴（故障注入场景 6） ===

/// 4 线程各 save 50 次不同 daily_goal_minutes。断言：
/// 1. 全部 POST 200（route 层 SETTINGS_WRITE 串行化读-改-写）；
/// 2. 风暴期间任何一次成功读取的 settings.json 都是合法 JSON（原子替换生效，
///    读者永远看不到半截文件）；
/// 3. 终值是某次写入的值，且能反序列化为合法 AppSettings。
#[test]
fn settings_concurrent_save_storm_always_valid_json() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let dir = tmpdir("save-storm");
    let db = std::sync::Arc::new(dir.join("kyn.db"));
    let stop = std::sync::Arc::new(AtomicBool::new(false));

    // 读者线程：风暴期间持续读取，任何成功读到的内容必须是完整合法 JSON
    let reader_stop = stop.clone();
    let reader_db = db.clone();
    let reader = std::thread::spawn(move || {
        let mut reads = 0usize;
        let path = settings::settings_path(&reader_db);
        while !reader_stop.load(Ordering::Relaxed) {
            if let Ok(text) = std::fs::read_to_string(&path) {
                reads += 1;
                assert!(
                    serde_json::from_str::<Value>(&text).is_ok(),
                    "并发写期间读到非法 JSON: {text}"
                );
            }
        }
        reads
    });

    let mut handles = Vec::new();
    for t in 0..4u32 {
        let db = db.clone();
        handles.push(std::thread::spawn(move || {
            let conn = mem_conn();
            for i in 0..50u32 {
                let goal = 60 + (t * 50 + i) % 1300; // 合法域 0..=1440 内各线程不同值
                let body = format!(r#"{{"daily_goal_minutes":{goal}}}"#);
                let (code, _, out) = route_req(&conn, "POST", "/api/settings", &body, &db);
                assert_eq!(code, 200, "线程 {t} 第 {i} 次保存失败: {out}");
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
    stop.store(true, Ordering::Relaxed);
    let reads = reader.join().unwrap();
    assert!(reads > 0, "读者线程必须至少完成一次读取");

    // 终值：合法 JSON + 是写入集内的值 + 反序列化为 AppSettings
    let text = std::fs::read_to_string(settings::settings_path(&db)).unwrap();
    let v: Value = serde_json::from_str(&text).unwrap();
    let goal = v["daily_goal_minutes"].as_u64().unwrap();
    assert!(
        (60..=1359).contains(&goal),
        "终值必须是某次写入的值: {goal}"
    );
    assert!(
        settings::try_load(&db).is_some(),
        "终值必须是合法 AppSettings"
    );
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
    let (code, _, body) = route_req(&conn, "GET", "/api/timeline?hours=nope", "", db);
    assert_eq!(
        code, 400,
        "hours 非法必须 400（Wave16：静默回退掩盖客户端 bug）"
    );
    assert!(body.contains("error"));
    let (code, _, _) = route_req(&conn, "GET", "/api/timeline?hours=-5", "", db);
    assert_eq!(code, 400, "负 hours 同样 400");

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
    // 注入"今天 23:30"作为当前时刻：与 local_ts 的 2026-09-09 锚点同基准，
    // 窗口推导不随真实日历漂移。
    let now = chrono::DateTime::parse_from_rfc3339(&local_ts(0, 23, 30))
        .unwrap()
        .with_timezone(&chrono::Local);
    let v = api_insights_at(&conn, 2, now);
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

// === insights：P1 驻留封顶 / P3 黄金时段 / 文案门槛 ===

#[test]
fn insights_dwell_caps_single_segment_at_30_minutes() {
    let conn = mem_conn();
    // 跨天空档：appA 切入后 72 小时才有下一次切换——单段必须截到 30 分钟，
    // 不再输出 "appA 72.0h" 级别的荒谬值。
    insert(&conn, &local_ts(-6, 0, 1), "window", "switch", Some("appA"));
    insert(&conn, &local_ts(-3, 0, 1), "window", "switch", Some("appB"));
    // 过 50 事件门槛（50 条输入）
    for _ in 0..50 {
        insert(&conn, &local_ts(-1, 12, 0), "keyboard", "press", None);
    }
    let now = chrono::DateTime::parse_from_rfc3339(&local_ts(0, 23, 30))
        .unwrap()
        .with_timezone(&chrono::Local);
    let v = api_insights_at(&conn, 2, now);
    let dwell_card = v["insights"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["title_en"] == json!("Top 3 apps by dwell time"))
        .expect("驻留卡应存在");
    let en = dwell_card["text_en"].as_str().unwrap();
    assert!(en.contains("appA 0.5h"), "72h 空档应截断为 30 分钟: {en}");
    assert!(!en.contains("72"), "不得出现未封顶时长: {en}");
}

#[test]
fn insights_golden_hours_pair_midnight_across_day_boundary() {
    let conn = mem_conn();
    // 23 点与 0 点各 40 条：跨午夜组合 (23:00-01:00) 必须当选；
    // 旧实现 for h in 0..23 永远看不到 h=23 的配对。
    for _ in 0..40 {
        insert(&conn, &local_ts(-1, 0, 30), "keyboard", "press", None);
        insert(&conn, &local_ts(-1, 23, 30), "keyboard", "press", None);
    }
    // 干扰项：9/10 点各 30 条（各不足总数 140 的 25%）
    for _ in 0..30 {
        insert(&conn, &local_ts(-1, 9, 30), "keyboard", "press", None);
        insert(&conn, &local_ts(-1, 10, 30), "keyboard", "press", None);
    }
    let now = chrono::DateTime::parse_from_rfc3339(&local_ts(0, 23, 30))
        .unwrap()
        .with_timezone(&chrono::Local);
    let v = api_insights_at(&conn, 2, now);
    let golden = v["insights"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["title_en"] == json!("Your golden hours"))
        .expect("黄金时段卡应存在");
    let zh = golden["text_zh"].as_str().unwrap();
    assert!(zh.contains("23:00-01:00"), "跨午夜组合应当选: {zh}");
}

#[test]
fn insights_golden_hours_requires_each_hour_at_least_quarter_of_total() {
    let conn = mem_conn();
    // 小时 0 密集（60）+ 小时 1 全空：不带 25% 门槛时 (0,1) 组合和 60 会
    // 以更早的 h=0 胜出，把"密集小时 + 空小时"拼成"最密集两小时"。
    // 门槛下 (0,1) 出局，由 9/10 点（各 30，恰好 >= 总数 120 的 25%）当选。
    for _ in 0..60 {
        insert(&conn, &local_ts(-1, 0, 30), "keyboard", "press", None);
    }
    for _ in 0..30 {
        insert(&conn, &local_ts(-1, 9, 30), "keyboard", "press", None);
        insert(&conn, &local_ts(-1, 10, 30), "keyboard", "press", None);
    }
    let now = chrono::DateTime::parse_from_rfc3339(&local_ts(0, 23, 30))
        .unwrap()
        .with_timezone(&chrono::Local);
    let v = api_insights_at(&conn, 2, now);
    let golden = v["insights"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["title_en"] == json!("Your golden hours"))
        .expect("黄金时段卡应存在");
    let zh = golden["text_zh"].as_str().unwrap();
    assert!(
        zh.contains("09:00-11:00") && !zh.contains("00:00-02:00"),
        "密集+空小时拼凑的组合不得当选，应选双密集小时: {zh}"
    );
}

#[test]
fn insights_late_night_uses_23_to_6_window() {
    let conn = mem_conn();
    // 深夜窗口统一为 [23:00, 次日 06:00)：23:30 的输入必须计入（旧口径
    // h < 6 会漏掉 23 点段），凌晨 05:00 同样计入；合计 60 > 30 门槛出卡。
    for _ in 0..40 {
        insert(&conn, &local_ts(-1, 23, 30), "keyboard", "press", None);
    }
    for _ in 0..20 {
        insert(&conn, &local_ts(0, 5, 0), "keyboard", "press", None);
    }
    let now = chrono::DateTime::parse_from_rfc3339(&local_ts(0, 23, 30))
        .unwrap()
        .with_timezone(&chrono::Local);
    let v = api_insights_at(&conn, 2, now);
    let card = v["insights"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["title_en"] == json!("Late-night activity"))
        .expect("23:30 与 05:00 合计 60 次，深夜卡应存在");
    let zh = card["text_zh"].as_str().unwrap();
    let en = card["text_en"].as_str().unwrap();
    assert!(zh.contains("23:00-06:00"), "文案应注明统一窗口: {zh}");
    assert!(
        zh.contains("60 次"),
        "23 点段的 40 次应与凌晨 20 次合并计数: {zh}"
    );
    assert!(en.contains("23:00-06:00"), "en 侧同步: {en}");
}

#[test]
fn insights_below_gate_with_today_data_shows_warming_up_card() {
    let conn = mem_conn();
    // 今日已有 10 条输入但未到 50 门槛：给"数据积累中"info 卡而非空列表
    for _ in 0..10 {
        insert(&conn, &local_ts(0, 10, 0), "keyboard", "press", None);
    }
    let now = chrono::DateTime::parse_from_rfc3339(&local_ts(0, 23, 30))
        .unwrap()
        .with_timezone(&chrono::Local);
    let v = api_insights_at(&conn, 2, now);
    let list = v["insights"].as_array().unwrap();
    assert_eq!(list.len(), 1, "应恰好一张 warming-up 卡: {v}");
    assert_eq!(list[0]["title_en"], json!("Still warming up"));
    assert_eq!(list[0]["title_zh"], json!("数据积累中"));
    assert!(
        list[0]["text_en"]
            .as_str()
            .unwrap()
            .contains("10 events so far"),
        "en 文案应带事件数"
    );
    assert!(
        list[0]["text_zh"]
            .as_str()
            .unwrap()
            .contains("已记录 10 条"),
        "zh 文案应带事件数"
    );
}

#[test]
fn insights_empty_db_returns_empty_list() {
    let conn = mem_conn();
    let now = chrono::DateTime::parse_from_rfc3339(&local_ts(0, 23, 30))
        .unwrap()
        .with_timezone(&chrono::Local);
    let v = api_insights_at(&conn, 2, now);
    assert!(
        v["insights"].as_array().unwrap().is_empty(),
        "完全无数据时仍为空列表（不出 warming-up 卡）: {v}"
    );
}

// === anomalies：message_en 映射 ===

#[test]
fn anomalies_message_en_covers_all_kinds() {
    // 与 kynoptic-core anomaly.rs 的中文模板逐字同源（数字全带上）
    let late = anomaly_message_en("late_night", "深夜活动：240 按键", None);
    assert!(late.contains("Late-night"), "{late}");
    assert!(late.contains("240"), "message_en 应带上按键数: {late}");

    let burst = anomaly_message_en(
        "apm_burst",
        "APM 突增：2026-09-17T10:23 达到 87（历史均值 12 的 7.3x）",
        Some("2026-09-17T10:23"),
    );
    assert!(burst.contains("APM burst"), "{burst}");
    assert!(burst.contains("87"), "message_en 应带上峰值 APM: {burst}");
    assert!(burst.contains("7.3x"), "message_en 应带上倍率: {burst}");
    assert!(
        burst.contains("2026-09-17T10:23"),
        "message_en 应带上分钟: {burst}"
    );

    let marathon = anomaly_message_en("marathon", "马拉松会话：连续活跃 195 分钟", None);
    assert!(marathon.contains("Marathon"), "{marathon}");
    assert!(
        marathon.contains("195"),
        "message_en 应带上分钟数: {marathon}"
    );

    let surge = anomaly_message_en(
        "new_app_surge",
        "应用使用突增：code.exe（今天 800，日均 60，13.2x）",
        None,
    );
    assert!(surge.contains("surge"), "{surge}");
    assert!(
        surge.contains("code.exe"),
        "message_en 应带上应用名: {surge}"
    );
    assert!(
        surge.contains("800"),
        "message_en 应带上今日事件数: {surge}"
    );
    assert!(surge.contains("13.2x"), "message_en 应带上倍率: {surge}");

    let fresh = anomaly_message_en("new_app_surge", "新应用首次出现：foo.exe（120 事件）", None);
    assert!(fresh.contains("first seen"), "{fresh}");
    assert!(
        fresh.contains("foo.exe") && fresh.contains("120"),
        "{fresh}"
    );

    assert!(anomaly_message_en("unknown_kind", "未知异常", None).contains("unknown_kind"));
}

// === trends：presence_minutes 假别名已移除（第四口径修复） ===

#[test]
fn trends_no_presence_alias_and_notes_raw_metric() {
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
    // 假别名彻底移除：active_minutes 是 raw 输入口径，不得冒充"人在场"
    assert!(
        d.get("presence_minutes").is_none(),
        "trends 不得再有 presence_minutes 假别名: {d}"
    );
    assert!(v["this_week"].get("presence_minutes").is_none());
    // note 字段说明口径并指向权威入口
    let note = v["note"].as_str().unwrap();
    assert!(note.contains("active_minutes"), "{note}");
    assert!(note.contains("/api/overview"), "{note}");
}

/// 口径（统一 2026-09）：sum7 窗口固定 7 个日历日，daily_agg 缺行按 0 计。
///
/// 本周（09-10..09-16）缺 09-13 一天：正确合计 = 6 天 × 100 = 600；
/// 修复前按行号切片会把窗口前移一天"吃进"09-09 的 100 → 误得 700。
#[test]
fn trends_sum7_aligned_by_calendar_date_zero_fills_missing_days() {
    let conn = mem_conn();
    for d in 3..=16u32 {
        if d == 13 {
            continue; // 本周中间缺 09-13 一天
        }
        conn.execute(
            "INSERT INTO daily_agg (date, keys, clicks, active_minutes) \
             VALUES (?1, 100, 10, 5)",
            params![format!("2026-09-{d:02}")],
        )
        .unwrap();
    }
    let today = chrono::NaiveDate::from_ymd_opt(2026, 9, 16).unwrap();
    let v = api_trends_at(&conn, today);
    assert_eq!(v["this_week"]["keys"], json!(600), "缺日按 0 计: {v}");
    assert_eq!(v["this_week"]["active_minutes"], json!(30));
    // 上周（09-03..09-09）7 天全有数据：不受本周缺日影响
    assert_eq!(v["last_week"]["keys"], json!(700));
    assert_eq!(v["daily"].as_array().unwrap().len(), 13);
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

    // 故障注入场景 4 加严：100 并发 + 30 个恶意慢速（slowloris：只发半个请求，
    // 占满服务端 5s read_timeout 的连接名额）。并发上限 64，因此 30 个慢速占位
    // 期间必然有部分正常请求按设计收到 503；断言：
    //   1. 全部 70 个正常请求在 10 秒内得到确定的 HTTP 响应（200 或 503，不悬挂）；
    //   2. 至少部分正常请求被真正服务（200）且服务存活；
    //   3. 慢速客户端断开、名额归还（Drop 守卫）后，连续请求不再出现 503。
    #[test]
    fn hundred_concurrent_with_slowloris_no_hang_and_quota_recovers() {
        let (port, dir) = start_server("stress100");
        // 30 个恶意慢速：发半个请求后挂住（服务端读超时 5s 才释放名额）
        let mut slow = Vec::new();
        for _ in 0..30 {
            let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
            s.set_write_timeout(Some(Duration::from_secs(5))).unwrap();
            s.write_all(b"GET /api/status HT").unwrap();
            slow.push(s);
        }
        std::thread::sleep(Duration::from_millis(300)); // 让慢速先占满名额

        let started = std::time::Instant::now();
        let retried = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let handles: Vec<_> = (0..70)
            .map(|_| {
                let retried = retried.clone();
                std::thread::spawn(move || {
                    let deadline = std::time::Instant::now() + Duration::from_secs(10);
                    loop {
                        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
                        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
                        s.set_write_timeout(Some(Duration::from_secs(5))).unwrap();
                        s.write_all(b"GET /api/status HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
                            .unwrap();
                        let mut buf = Vec::new();
                        let _ = s.read_to_end(&mut buf);
                        let resp = String::from_utf8_lossy(&buf).to_string();
                        let code: u16 = resp
                            .split_whitespace()
                            .nth(1)
                            .and_then(|c| c.parse().ok())
                            .unwrap_or(0);
                        if code != 0 {
                            return resp;
                        }
                        // 100 并发风暴下 Windows 回环偶发连接被截断（RST）：
                        // 在 10s 截止前重试，截断次数记录供报告
                        retried.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        assert!(
                            std::time::Instant::now() < deadline,
                            "10 秒内未得到完整 HTTP 响应（悬挂）: {resp}"
                        );
                        std::thread::sleep(Duration::from_millis(50));
                    }
                })
            })
            .collect();
        let mut codes = Vec::new();
        for h in handles {
            let resp = h.join().unwrap();
            let code: u16 = resp
                .split_whitespace()
                .nth(1)
                .and_then(|c| c.parse().ok())
                .unwrap_or(0);
            assert!(code != 0, "10 秒内必须得到完整 HTTP 响应（不悬挂）: {resp}");
            codes.push(code);
        }
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "70 个正常请求必须在 10 秒内全部完成"
        );
        assert_eq!(
            codes.iter().filter(|&&c| c == 200).count() as i64
                + codes.iter().filter(|&&c| c == 503).count() as i64,
            70,
            "每个响应必须是 200 或设计内 503: {codes:?}"
        );
        let served = codes.iter().filter(|&&c| c == 200).count();
        assert!(served > 0, "至少部分正常请求应被服务（200）: {codes:?}");
        eprintln!(
            "DBG slowloris: served200={served} rejected503={} connTruncatedRetries={}",
            70 - served,
            retried.load(std::sync::atomic::Ordering::Relaxed)
        );

        // 释放慢速客户端，等服务端 5s 读超时回收名额（InflightGuard Drop 归还）
        drop(slow);
        std::thread::sleep(Duration::from_millis(6500));
        // 名额归零：连续请求全部 200，不再 503
        for _ in 0..30 {
            let resp = get(port, "GET /api/status HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n");
            assert!(
                resp.starts_with("HTTP/1.1 200 "),
                "名额归还后不应再 503（服务必须存活）: {resp}"
            );
        }
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

    #[test]
    fn oversized_body_gets_413_not_silent_truncation() {
        let (port, dir) = start_server("body-413");
        // fuzz 实证：Content-Length > 64KB 旧实现静默截断到 64KB 继续解析。
        // 现在必须直接 413（带安全头、Connection: close），不读 body。
        let req = format!(
            "POST /api/settings HTTP/1.1\r\n\
             Host: 127.0.0.1:{port}\r\n\
             Origin: http://127.0.0.1:{port}\r\n\
             X-Kynoptic: 1\r\n\
             Content-Length: 70000\r\n\
             \r\n"
        );
        let resp = get(port, &req);
        assert!(
            resp.starts_with("HTTP/1.1 413 "),
            "超限 body 必须回 413: {resp}"
        );
        assert!(resp.contains("nosniff"), "413 必须带安全头: {resp}");
        assert!(
            resp.contains("Connection: close"),
            "413 必须声明关闭连接: {resp}"
        );
        // 服务存活：正常请求仍 200
        let resp = get(
            port,
            &format!("GET /api/status HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n"),
        );
        assert!(
            resp.starts_with("HTTP/1.1 200 "),
            "413 后服务必须存活: {resp}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
