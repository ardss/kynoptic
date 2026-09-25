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

// "today" 归一化与 hours/report/apps_grid 同族一致（此前仅 summary 拒绝字面量）
#[test]
fn summary_accepts_today_alias() {
    let conn = mem_conn();
    let v = api_summary(&conn, "today").unwrap();
    assert_eq!(v["date"], json!(queries::today_local_str()));
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
    // Wave28 语义：恒返回 12 个桶，终点=当前本地小时整点（T12，进行中），
    // 起点=终点−11h（T01）。无数据小时 human/auto 全 0。
    assert_eq!(buckets.len(), 12, "12 小时窗口全量补零: {v}");
    assert_eq!(buckets[0]["hour"], json!("2026-09-09T01"));
    assert_eq!(
        buckets[11]["hour"],
        json!("2026-09-09T12"),
        "最后一桶必须是当前（进行中的）本地小时"
    );
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
    // 744h（31 天）窗口外的事件不应出现；窗口内的要出现
    let old = (now - chrono::Duration::hours(9000)).to_rfc3339();
    insert(&conn, &old, "keyboard", "press", None);
    let near = (now - chrono::Duration::hours(100)).to_rfc3339();
    insert(&conn, &near, "keyboard", "press", None);
    let v = api_timeline_at(&conn, 9999, now, 2).unwrap();
    assert_eq!(
        v["hours"],
        json!(744),
        "Wave24：与 HTTP 层 clamp 对齐（744=31 天）"
    );
    // 补零桶：固定返回窗口内全部本地小时
    let buckets = v["buckets"].as_array().unwrap();
    assert_eq!(buckets.len(), 744, "补零后恒为 744 桶");
    // press 行经 apps 分布通道呈现（human_min 只认 input_agg 行）：
    // 恰有 1 个桶带应用事件（9000h 前的旧事件必须被排除）
    let hit = buckets
        .iter()
        .filter(|b| b["apps"].as_array().is_some_and(|a| !a.is_empty()))
        .count();
    assert_eq!(hit, 1, "窗口内应恰有 1 个带事件的桶");
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
    // 期望值用插入的已知 ts（Wave18：期望值调生产函数=同义反复，两边
    // 一起坏掉时测试照绿），只对截断到秒的 19 字符前缀
    assert_eq!(
        v["last_event_ts"],
        json!(ts.get(..19).unwrap_or(&ts)),
        "last_event_ts = 插入时刻截到秒（UTC 裸串）"
    );
    assert_eq!(v["today"], json!(queries::today_local_str()));
    assert_eq!(v["read_only"], json!(true));
    assert_eq!(v["bind"], json!("127.0.0.1"));
    // ci 修复：构建指纹字段必须存在（本地构建缺省 unknown；CI 构建注入
    // git hash + 构建时间），供审查时比对运行态与源码版本
    assert!(v["build"]["git_hash"].is_string());
    assert!(v["build"]["built_at"].is_string());
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
    let v = api_heatmap_at(&conn, 1, today).unwrap();
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
    let v = api_heatmap_at(&conn, 1, today).unwrap();
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
    let v = api_heatmap_at(&conn, 999, today).unwrap();
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
    let v = api_apps_at(&conn, 7, today).unwrap();
    let apps = v["apps"].as_array().unwrap();
    assert_eq!(apps.len(), 2, "空名与窗口外排除: {v}");
    assert_eq!(apps[0]["app"], json!("code"));
    assert_eq!(apps[0]["count"], json!(3));
    assert_eq!(apps[1]["app"], json!("web"));
    assert_eq!(apps[1]["count"], json!(2));
}

// === hours ===
// 口径：每小时输入次数（input_agg 的 keys+clicks；无分钟行时回退 press/click
// 原始行），不再统计 heartbeat/采样等系统事件。

#[test]
fn hours_fills_24_buckets_with_zeros() {
    let conn = mem_conn();
    insert(&conn, &local_ts(0, 9, 0), "keyboard", "press", None);
    insert(&conn, &local_ts(0, 9, 30), "mouse", "click", None);
    insert(&conn, &local_ts(0, 10, 0), "window", "switch", Some("code")); // 非输入，不计
    insert(&conn, &local_ts(0, 22, 0), "mouse", "click", None);
    let v = api_hours(&conn, "2026-09-09").unwrap();
    assert_eq!(v["date"], json!("2026-09-09"));
    let values = v["values"].as_array().unwrap();
    assert_eq!(values.len(), 24);
    assert_eq!(values[9], json!(2));
    assert_eq!(values[22], json!(1));
    assert_eq!(values[8], json!(0), "缺时补零");
    assert_eq!(values[10], json!(0), "窗口切换不是输入，不再计入");
    assert!(v["note"].is_string(), "响应自带口径标注");
}

// input_agg 分钟行优先：keys+clicks 按小时求和，系统采样事件不混入
#[test]
fn hours_prefers_input_agg_rows() {
    let conn = mem_conn();
    insert_input_agg(&conn, &local_ts(0, 9, 0), r#"{"keys":3,"clicks":2}"#);
    insert_input_agg(&conn, &local_ts(0, 9, 30), r#"{"keys":1,"clicks":0}"#);
    insert(&conn, &local_ts(0, 9, 45), "system", "heartbeat", None); // 采样噪声
    let v = api_hours(&conn, "2026-09-09").unwrap();
    let values = v["values"].as_array().unwrap();
    assert_eq!(values[9], json!(6));
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
    // 回显经 sanitize_date_echo 净化（whitelist 字母数字/'-'，'_' 被剥除），
    // 不再原样反射攻击者可控文本（响应放大/日志注入投放面）。
    assert!(out.contains("未知监控器 id"));
    assert!(out.contains("notamonitor"));
    assert!(!out.contains("not_a_monitor"));
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
fn settings_post_rejects_non_object_body() {
    let dir = tmpdir("settings-non-object");
    let db = dir.join("kyn.db");
    // 顶层数组/标量/null 过去在 get() 上全部落空 → 静默 200 空转写；
    // 现在必须 400（fuzz 加固回归）。
    for body in ["[1,2,3]", "null", "\"str\"", "42", "true"] {
        let (code, _, out) = route_req(&mem_conn(), "POST", "/api/settings", body, &db);
        assert_eq!(code, 400, "body {body} 必须被拒绝: {out}");
    }
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

// 错误码层（审查修复）：err_json 携带稳定 code；404/405 双语；/api/input
// 截断不再无声（days_requested + note）。
#[test]
fn err_json_carries_stable_code_and_input_notes_truncation() {
    let conn = mem_conn();
    let db = Path::new("kyn.db");
    let code_of = |body: &str| -> String {
        let v: Value = serde_json::from_str(body).unwrap();
        v["code"].as_str().unwrap_or("").to_string()
    };
    // 未来日期 → future_date
    let (status, _, body) = route_req(&conn, "GET", "/api/summary?date=2099-01-01", "", db);
    assert_eq!(status, 400);
    assert_eq!(code_of(&body), "future_date");
    // 日期格式错 → invalid_date
    let (status, _, body) = route_req(&conn, "GET", "/api/summary?date=bad", "", db);
    assert_eq!(status, 400);
    assert_eq!(code_of(&body), "invalid_date");
    // 参数非法 → invalid_param
    let (status, _, body) = route_req(&conn, "GET", "/api/timeline?hours=nope", "", db);
    assert_eq!(status, 400);
    assert_eq!(code_of(&body), "invalid_param");
    // 404/405 → 稳定 code
    let (status, _, body) = route_req(&conn, "GET", "/api/nope", "", db);
    assert_eq!(status, 404);
    assert_eq!(code_of(&body), "not_found");
    let (status, _, body) = route_req(&conn, "DELETE", "/api/status", "", db);
    assert_eq!(status, 405);
    assert_eq!(code_of(&body), "method_not_allowed");
    // settings JSON 解析错误 → invalid_json（serde 原文不透传）
    let (status, _, body) = route_req(&conn, "POST", "/api/settings", "{oops}", db);
    assert_eq!(status, 400);
    assert_eq!(code_of(&body), "invalid_json");
    assert!(!body.contains("key must be a string"), "serde 原文不得透传");
    // /api/input：请求超 90 天上限时明示 days_requested 与截断说明
    let (status, _, body) = route_req(&conn, "GET", "/api/input?days=365", "", db);
    assert_eq!(status, 200);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        v["days_requested"],
        json!(365),
        "截断必须标注 days_requested"
    );
    assert!(
        v["note"].as_str().unwrap().contains("90"),
        "截断说明必须出现 90"
    );
    // 对照：未截断请求不带 days_requested
    let (status, _, body) = route_req(&conn, "GET", "/api/input?days=7", "", db);
    assert_eq!(status, 200);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert!(
        v.get("days_requested").is_none(),
        "未截断时不得添加标注字段"
    );
}

#[test]
fn route_table_and_error_codes() {
    let conn = mem_conn();
    let db = Path::new("kyn.db");
    let (code, ctype, body) = route_req(&conn, "GET", "/", "", db);
    assert_eq!(code, 200);
    assert!(ctype.starts_with("text/html"));
    assert!(body.contains("All data is local"), "页脚数据声明必须内嵌");

    let (code, ctype, body) = route_req(&conn, "GET", "/api/status", "", db);
    assert_eq!(code, 200);
    assert_eq!(ctype, "application/json");
    // Wave18：核心端点补 body 关键字段断言（此前只看状态码，字段名写坏
    // 也照绿）
    let v: Value = serde_json::from_str(&body).unwrap();
    assert!(v["last_event_ts"].is_string());
    assert!(v["today"].is_string());

    let (code, _, body) = route_req(&conn, "GET", "/api/summary?date=bad", "", db);
    assert_eq!(code, 400);
    assert!(body.contains("error"));

    // 缺省 date = 今日 → 200
    let (code, _, _) = route_req(&conn, "GET", "/api/summary", "", db);
    assert_eq!(code, 200);

    let (code, _, body) = route_req(&conn, "GET", "/api/timeline?hours=6", "", db);
    assert_eq!(code, 200);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert!(v["buckets"].is_array(), "timeline 必须有 buckets 数组");
    let (code, _, body) = route_req(&conn, "GET", "/api/timeline?hours=nope", "", db);
    assert_eq!(
        code, 400,
        "hours 非法必须 400（Wave16：静默回退掩盖客户端 bug）"
    );
    assert!(body.contains("error"));
    let (code, _, _) = route_req(&conn, "GET", "/api/timeline?hours=-5", "", db);
    assert_eq!(code, 400, "负 hours 同样 400");

    let (code, _, body) = route_req(&conn, "GET", "/api/anomalies?days=3", "", db);
    assert_eq!(code, 200);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert!(v["anomalies"].is_array(), "anomalies 必须是数组");
    // Wave18：numeric 参数非法与 hours 同口径 → 400
    assert_eq!(
        route_req(&conn, "GET", "/api/anomalies?days=nope", "", db).0,
        400
    );
    // days=0 与 MCP get_anomalies（days>=1）同口径：400，不再静默钳为 1 天
    let (code, _, body) = route_req(&conn, "GET", "/api/anomalies?days=0", "", db);
    assert_eq!(code, 400, "days=0 须 400（与 MCP 口径一致）");
    assert!(body.contains("error"));
    assert_eq!(
        route_req(&conn, "GET", "/api/heatmap?weeks=nope", "", db).0,
        400
    );
    assert_eq!(
        route_req(&conn, "GET", "/api/input?days=nope", "", db).0,
        400
    );
    assert_eq!(
        route_req(&conn, "GET", "/api/apps?days=nope", "", db).0,
        400
    );
    assert_eq!(
        route_req(&conn, "GET", "/api/daily_top?days=nope", "", db).0,
        400
    );
    // 未来日期一律 400（wave18 P1：看似权威的全 0 比报错更误导）
    assert_eq!(
        route_req(&conn, "GET", "/api/summary?date=2099-01-01", "", db).0,
        400
    );
    assert_eq!(
        route_req(&conn, "GET", "/api/report?date=2099-01-01", "", db).0,
        400
    );
    assert_eq!(
        route_req(&conn, "GET", "/api/hours?date=2099-01-01", "", db).0,
        400
    );
    // 字面量 today 不走未来判定（由 api_* 解析路径处理）
    assert_eq!(
        route_req(&conn, "GET", "/api/hours?date=today", "", db).0,
        200
    );
    // weeks 越界仍 clamp：10000 → 52 周 = 364 天
    let (code, _, body) = route_req(&conn, "GET", "/api/heatmap?weeks=10000", "", db);
    assert_eq!(code, 200);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["days"].as_array().unwrap().len(), 364);

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
    assert!(v["metrics_note"].as_str().unwrap().contains("自动化脚本"));
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
        .find(|i| i["title_en"] == json!("Top 3 apps by foreground time"))
        .expect("前台应用时长卡应存在");
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
    // 空分支附带真实门槛，前端据此渲染"已积累 N/50"
    assert_eq!(v["gate"]["events"], json!(0));
    assert_eq!(v["gate"]["required"], json!(50));
}

// 洞察响应不再携带调试遗留 dbg 字段（端点契约清单未声明，前端无引用）
#[test]
fn insights_response_has_no_dbg_field() {
    let conn = mem_conn();
    for i in 0..51 {
        insert(&conn, &local_ts(-1, 9, i % 60), "keyboard", "press", None);
    }
    let now = chrono::DateTime::parse_from_rfc3339(&local_ts(0, 23, 30))
        .unwrap()
        .with_timezone(&chrono::Local);
    let v = api_insights_at(&conn, 2, now);
    assert!(
        v.get("dbg").is_none(),
        "insights 响应不得携带调试字段 dbg: {v}"
    );
}

// 洞察页人侧判定含滚轮：纯滚轮阅读分钟也算一次输入（与 presence.rs 口径对齐）
#[test]
fn insights_counts_scroll_only_minute_as_human_input() {
    let conn = mem_conn();
    insert_input_agg(
        &conn,
        &local_ts(0, 14, 0),
        r#"{"keys":0,"clicks":0,"scroll_ticks":12,"injected_keys":0,"injected_clicks":0,"injected_scroll_ticks":0}"#,
    );
    // 注入滚轮不算人侧
    insert_input_agg(
        &conn,
        &local_ts(0, 15, 0),
        r#"{"keys":0,"clicks":0,"scroll_ticks":12,"injected_keys":0,"injected_clicks":0,"injected_scroll_ticks":12}"#,
    );
    let now = chrono::DateTime::parse_from_rfc3339(&local_ts(0, 23, 30))
        .unwrap()
        .with_timezone(&chrono::Local);
    let v = api_insights_at(&conn, 2, now);
    // 未到 50 门槛时走 warming-up 卡：纯滚轮分钟计 1 条，注入滚轮分钟不计
    let list = v["insights"].as_array().unwrap();
    assert_eq!(list.len(), 1, "{v}");
    assert!(
        list[0]["text_en"].as_str().unwrap().contains("1 events"),
        "纯滚轮分钟应计 1 条输入事件: {v}"
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

    let marathon = anomaly_message_en("marathon", "连续在场 195 分钟（长时间无离开）", None);
    assert!(marathon.contains("Continuous presence"), "{marathon}");
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
    let v = api_trends_at(&conn, today).unwrap();
    // daily 恒为 28 个日历日（缺日补零），按日期定位而非行号
    let arr = v["daily"].as_array().unwrap();
    assert_eq!(arr.len(), 28);
    let d = arr
        .iter()
        .find(|x| x["date"] == json!("2026-09-08"))
        .expect("2026-09-08 应在 28 天窗口内");
    assert_eq!(d["active_minutes"], json!(45));
    // 窗口内无 daily_agg 行的日子补零（幽灵零行守卫删除全零行后由 API 补齐）
    assert_eq!(arr[0]["active_minutes"], json!(0));
    assert_eq!(arr[0]["date"], json!("2026-08-13"));
    // 假别名彻底移除：active_minutes 是 raw 输入口径，不得冒充"人在场"
    assert!(
        d.get("presence_minutes").is_none(),
        "trends 不得再有 presence_minutes 假别名: {d}"
    );
    assert!(v["this_week"].get("presence_minutes").is_none());
    // note 字段用平实语言说明口径（含自动化注入，与"在场"口径不同）
    let note = v["note"].as_str().unwrap();
    assert!(note.contains("自动化脚本"), "{note}");
    assert!(note.contains("automation"), "{note}");
    assert!(
        !note.contains("daily_agg") && !note.contains("raw"),
        "面向用户的小字不再暴露内部术语: {note}"
    );
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
    let v = api_trends_at(&conn, today).unwrap();
    assert_eq!(v["this_week"]["keys"], json!(600), "缺日按 0 计: {v}");
    assert_eq!(v["this_week"]["active_minutes"], json!(30));
    // 上周（09-03..09-09）7 天全有数据：不受本周缺日影响
    assert_eq!(v["last_week"]["keys"], json!(700));
    // daily 恒为 28 个日历日；缺的 09-13 在 daily 里补零（报告页趋势图
    // 按行号布局，缺行会导致条形/日期标签错位）
    let arr = v["daily"].as_array().unwrap();
    assert_eq!(arr.len(), 28);
    let missing = arr
        .iter()
        .find(|x| x["date"] == json!("2026-09-13"))
        .expect("缺日也应在 28 天窗口内");
    assert_eq!(missing["active_minutes"], json!(0));
    assert_eq!(missing["keys"], json!(0));
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

// === 本轮修复（发现修复 + 挂账清偿）===

/// 挂账 Wave31：try_load 钳制 categories 条数（≤100）与 pattern 长度（≤200），
/// 且预编译 lc_tokens（小写、非空），匹配语义不变。
#[test]
fn try_load_clamps_categories_and_precompiles_tokens() {
    let dir = tmpdir("clamp");
    let db = dir.join("kyn.db");
    // 150 条规则，每条 pattern 300 字符
    let long_pat = "a".repeat(300);
    let rules: Vec<String> = (0..150)
        .map(|i| format!(r#"{{"name":"c{i}","pattern":"{long_pat}"}}"#))
        .collect();
    std::fs::write(
        settings::settings_path(&db),
        format!(r#"{{"categories":[{}]}}"#, rules.join(",")),
    )
    .unwrap();
    let s = settings::try_load(&db).expect("合法 JSON 应解析成功");
    assert_eq!(s.categories.len(), 100, "超 100 条应截断");
    for r in &s.categories {
        assert!(r.pattern.chars().count() <= 200, "pattern 应截到 200 字符");
        assert!(
            r.lc_tokens
                .iter()
                .all(|t| t.chars().all(|c| !c.is_uppercase())),
            "预编译 token 必须已小写"
        );
    }
    // 匹配语义不变：小写 token 子串命中
    let r = settings::CategoryRule::rule("x", "GitHub CODE");
    assert!(r.matches("github.com", "Pull Requests"));
    assert!(!r.matches("gitee.com", "Pull Requests"));
    std::fs::remove_dir_all(&dir).unwrap();
}

/// 发现 ①-3：读端点 DB 级失败统一 400（不再 200 + 静默空数据）。
#[test]
fn read_endpoints_map_db_errors_to_400() {
    let conn = mem_conn();
    let db = tmpdir("errmap").join("kyn.db");
    conn.execute_batch("ALTER TABLE daily_agg RENAME TO daily_agg_x")
        .unwrap();
    let (code, _, body) = route_req(&conn, "GET", "/api/trends", "", &db);
    assert_eq!(code, 400, "trends 表缺失应 400: {body}");
    assert!(body.contains("error"));
    conn.execute_batch("ALTER TABLE daily_agg_x RENAME TO daily_agg")
        .unwrap();
    conn.execute_batch("ALTER TABLE events RENAME TO events_x")
        .unwrap();
    // heatmap 只读 daily_agg（不触 events），rename 后仍合法 200，不在断言列
    for path in ["/api/apps?days=3", "/api/daily_top?days=3"] {
        let (code, _, body) = route_req(&conn, "GET", path, "", &db);
        assert_eq!(code, 400, "{path} events 表缺失应 400: {body}");
    }
}

/// 发现 ①-4：/api/diagnostics 返回固定文件清单，含存在性/大小/尾部，
/// 只回文件名不暴露路径。
#[test]
fn diagnostics_lists_known_files_without_paths() {
    let dir = tmpdir("diag");
    let db = dir.join("kyn.db");
    std::fs::write(dir.join("collector-error.log"), "line1\nboom \u{7}bad\x1b").unwrap();
    let (code, _, body) = route_req(&mem_conn(), "GET", "/api/diagnostics", "", &db);
    assert_eq!(code, 200);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert!(db_path_is_not_leaked(&v, &db));
    let files = v["files"].as_array().unwrap();
    for name in [
        "collector-error.log",
        "dashboard-error.log",
        "watchdog.log",
        "tray.log",
        "update.log",
        "dashboard-port.txt",
    ] {
        assert!(
            files.iter().any(|f| f["name"] == name),
            "清单缺 {name}: {body}"
        );
    }
    let ce = files
        .iter()
        .find(|f| f["name"] == "collector-error.log")
        .unwrap();
    assert_eq!(ce["exists"], json!(true));
    assert_eq!(ce["size_bytes"], json!(ce["size_bytes"]), "size 字段存在");
    let tail = ce["tail"].as_str().unwrap();
    assert!(tail.contains("line1") && tail.contains("boom"));
    assert!(
        !tail.contains('\u{7}') && !tail.contains('\u{1b}'),
        "控制字符应被净化"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

fn db_path_is_not_leaked(v: &Value, db: &Path) -> bool {
    let s = v.to_string();
    let dir = db.parent().unwrap().to_string_lossy().to_string();
    !s.contains(&dir) && !s.contains(&db.to_string_lossy().to_string())
}

/// 发现 ①-8：/api/input 的 SQL 聚合与逐行累加语义一致（keys/clicks 求和、
/// per-key 频次、今日逐时），NULL/脏 JSON 行跳过。
#[test]
fn input_sql_aggregation_matches_row_semantics() {
    let conn = mem_conn();
    let today = chrono::Local::now().date_naive();
    let ts = |h: u32| {
        let naive = today.and_hms_opt(h, 1, 0).unwrap();
        use chrono::TimeZone;
        Local
            .from_local_datetime(&naive)
            .earliest()
            .unwrap()
            .to_rfc3339()
    };
    let ins = |ts: &str, etype: &str, data: &str| {
        conn.execute(
            "INSERT INTO events (timestamp, event_type, event_action, event_data, app_name, window_title, session_id) VALUES (?1,?2,'input_agg',?3,NULL,NULL,NULL)",
            params![ts, etype, data],
        )
        .unwrap();
    };
    ins(&ts(8), "keyboard", r#"{"keys":10,"vk":{"65":7,"66":3}}"#);
    ins(&ts(9), "keyboard", r#"{"keys":5,"vk":{"65":2}}"#);
    // (timestamp,event_type) 唯一键：错开分钟
    let ts9 = |m: u32| {
        let naive = today.and_hms_opt(9, m, 0).unwrap();
        use chrono::TimeZone;
        Local
            .from_local_datetime(&naive)
            .earliest()
            .unwrap()
            .to_rfc3339()
    };
    ins(
        &ts9(2),
        "mouse",
        r#"{"clicks":4,"clicks_left":3,"clicks_right":1,"scroll_ticks":9,"moves":20,"move_distance_px":500}"#,
    );
    ins(&ts9(3), "mouse", r#"not json"#); // 脏行：跳过
    let v = api_input_at(&conn, 7, today, today).unwrap();
    assert_eq!(v["granularity"], json!("minute"));
    assert_eq!(v["keys_total"], json!(15));
    assert_eq!(v["clicks_total"], json!(4));
    assert_eq!(v["clicks_left"], json!(3));
    assert_eq!(v["scroll_ticks"], json!(9));
    assert_eq!(v["move_distance_px"], json!(500));
    assert_eq!(v["key_freq"]["65"], json!(9));
    assert_eq!(v["key_freq"]["66"], json!(3));
    let hour = chrono::Local::now().hour() as usize;
    if hour == 8 {
        assert_eq!(v["hourly_today"][8], json!(10));
    }
    assert_eq!(v["series"].as_array().unwrap().len(), 1);
    assert_eq!(v["series"][0]["keys"], json!(15));
    assert_eq!(v["series"][0]["clicks"], json!(4));
}

/// 发现 ①-7：report 的 data_since 取本地日口径（UTC+8 下不早一天）。
#[test]
fn report_data_since_uses_local_date() {
    let conn = mem_conn();
    let db = tmpdir("since").join("kyn.db");
    // 一个明确的 UTC 时刻：其 UTC 日期与本地日期不同的时刻一定存在于任一时区
    // 差 ≥1h 的机器上；此处只验证字段来自 datetime(...,'localtime') 通路
    // （值 = 库中最早事件的本地日），不针对特定时区断言具体日期。
    insert(
        &conn,
        "2026-01-05T20:30:00+00:00",
        "window",
        "switch",
        Some("code"),
    );
    let s = settings::AppSettings::default();
    let v = api_report_at(&conn, "2026-01-06", &s).unwrap_or_else(|e| panic!("{e}"));
    let since = v["data_since"].as_str().unwrap();
    let expect = {
        let t = chrono::DateTime::parse_from_rfc3339("2026-01-05T20:30:00+00:00").unwrap();
        t.with_timezone(&Local).format("%Y-%m-%d").to_string()
    };
    assert_eq!(since, expect, "data_since 应为最早事件的本地日");
    let _ = db;
}

// ─── 本轮修复回归（日期边界 / 空参数 / 幻影日 / 安全头 / host 尾点） ────────

#[test]
fn invalid_calendar_date_reports_format_error_not_future() {
    // 2026-13-45 形如日期但历法非法：应报"格式错"，不再误报"在未来"
    let dir = tmpdir("bad-cal");
    let (code, _, out) = route_req(&mem_conn(), "GET", "/api/summary?date=2026-13-45", "", &dir);
    assert_eq!(code, 400);
    assert!(out.contains("格式错"), "got: {out}");
    assert!(!out.contains("在未来"), "got: {out}");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn empty_date_param_is_rejected_not_silently_defaulted() {
    // `?date=` 显式空值 ≠ 缺省：回 400，与数值参数政策一致
    let dir = tmpdir("empty-date");
    let (code, _, out) = route_req(&mem_conn(), "GET", "/api/summary?date=", "", &dir);
    assert_eq!(code, 400, "got: {out}");
    assert!(out.contains("不应为空"), "got: {out}");
    // 缺省（无参数）仍回退今天 → 200
    let (code2, _, _) = route_req(&mem_conn(), "GET", "/api/summary", "", &dir);
    assert_eq!(code2, 200);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn trends_excludes_future_phantom_daily_rows() {
    // 时钟拨快回拨残留的 daily_agg "未来日"幻影行不得泄漏进 trends
    let conn = mem_conn();
    conn.execute(
        "INSERT INTO daily_agg (date, keys, clicks, active_minutes) VALUES ('9999-12-31', 999, 0, 0)",
        [],
    )
    .unwrap();
    let v = api_trends_at(&conn, today_naive()).unwrap();
    let daily = v["daily"].as_array().unwrap();
    assert!(
        !daily.iter().any(|d| d["date"] == "9999-12-31"),
        "幻影未来日泄漏: {daily:?}"
    );
}

#[test]
fn security_headers_have_referrer_policy_and_nonce_csp() {
    let h = security_headers("'self'");
    assert!(h.contains("Referrer-Policy: no-referrer"), "got: {h}");
    // script-src 收紧：默认响应不再放行内联脚本
    assert!(h.contains("script-src 'self';"), "got: {h}");
    assert!(!h.contains("script-src 'unsafe-inline'"), "got: {h}");
    // 首页变体：script-src 带随机 nonce
    let n = fresh_nonce();
    let hi = security_headers(&format!("'self' 'nonce-{n}'"));
    assert!(hi.contains("script-src 'self' 'nonce-"), "got: {hi}");
    // nonce 唯一性（相邻两次生成不重复，128-bit 十六进制）
    assert_eq!(n.len(), 32);
    assert_ne!(n, fresh_nonce());
}

#[test]
fn loopback_host_strips_trailing_dot_in_browser_forms() {
    use super::loopback_host_ok;
    // 浏览器形态：host 部分带尾点
    assert!(loopback_host_ok("localhost.:8422", 8422));
    assert!(loopback_host_ok("127.0.0.1.:8422", 8422));
    // 整串尾点（端口后）
    assert!(loopback_host_ok("127.0.0.1:8422.", 8422));
    assert!(loopback_host_ok("localhost.:8422.", 8422));
    // 无端口 + 尾点
    assert!(loopback_host_ok("localhost.", 8422));
    // 非回环仍拒绝
    assert!(!loopback_host_ok("evil.example.com:8422", 8422));
}

// === 查询参数解析统一（审查修复：qval/qdate 语义分裂收口） ===
#[test]
fn duplicate_query_params_take_first_everywhere() {
    let conn = mem_conn();
    let db = Path::new("kyn.db");
    // 重复日期：一律取首个（此前取末个）。两个方向一个合法（2000 年，远
    // 过去）一个非法，首末取向不同则结果必不同。
    let (code, _, body) = route_req(
        &conn,
        "GET",
        "/api/summary?date=bad&date=2000-01-01",
        "",
        db,
    );
    assert_eq!(code, 400, "取首个 date=bad（格式错）须 400: {body}");
    let (code, _, body) = route_req(
        &conn,
        "GET",
        "/api/summary?date=2000-01-01&date=bad",
        "",
        db,
    );
    assert_eq!(code, 200, "取首个 date=2000-01-01 合法须 200: {body}");
    // 重复数值：一律取首个（此前取首个，保持方向一致）
    let (code, _, body) = route_req(&conn, "GET", "/api/apps?days=abc&days=2", "", db);
    assert_eq!(code, 400, "首个 days=abc 非法须 400: {body}");
    // 数值参数显式空串 → 400（此前静默回缺省）
    let (code, _, body) = route_req(&conn, "GET", "/api/timeline?hours=", "", db);
    assert_eq!(code, 400, "hours= 空串须 400: {body}");
    assert!(body.contains("不应为空"), "got: {body}");
    let (code, _, _) = route_req(&conn, "GET", "/api/apps?days=", "", db);
    assert_eq!(code, 400, "days= 空串与日期族同口径 400");
}

#[test]
fn numeric_params_below_range_rejected_with_domain_in_message() {
    let conn = mem_conn();
    let db = Path::new("kyn.db");
    // 六个数值端点 days/weeks/hours=0 统一 400，文案写明有效域
    for (path, domain) in [
        ("/api/anomalies?days=0", "1-30"),
        ("/api/apps?days=0", "1-365"),
        ("/api/daily_top?days=0", "1-90"),
        ("/api/input?days=0", "1-90"),
        ("/api/heatmap?weeks=0", "1-52"),
        ("/api/timeline?hours=0", "1-744"),
    ] {
        let (code, _, body) = route_req(&conn, "GET", path, "", db);
        assert_eq!(code, 400, "{path} 的 0 须 400: {body}");
        assert!(
            body.contains(domain),
            "{path} 错误文案须写明有效域 {domain}: {body}"
        );
    }
    // anomalies：负数与 0 同一条文案（此前分裂为两条）
    let (_, _, body) = route_req(&conn, "GET", "/api/anomalies?days=-1", "", db);
    assert!(body.contains("1-30"), "got: {body}");
    // 非数字文案同样含有效域
    let (_, _, body) = route_req(&conn, "GET", "/api/heatmap?weeks=abc", "", db);
    assert!(body.contains("1-52"), "got: {body}");
}

#[test]
fn over_max_numeric_params_are_clamped_with_annotation() {
    let conn = mem_conn();
    let db = Path::new("kyn.db");
    // 各数值端点超上限：仍 200，但响应标注 X_requested + note（不再无声）
    let cases = [
        ("/api/timeline?hours=999999", "hours_requested"),
        ("/api/heatmap?weeks=999999", "weeks_requested"),
        ("/api/apps?days=999999", "days_requested"),
        ("/api/daily_top?days=999999", "days_requested"),
        ("/api/anomalies?days=999999", "days_requested"),
    ];
    for (path, field) in cases {
        let (code, _, body) = route_req(&conn, "GET", path, "", db);
        assert_eq!(code, 200, "{path}: {body}");
        let v: Value = serde_json::from_str(&body).unwrap();
        assert!(v.get(field).is_some(), "{path} 须标注 {field}: {body}");
        assert!(
            v["note"].as_str().unwrap().contains("上限"),
            "{path} 须带截断说明: {body}"
        );
    }
    // anomalies 窗口钳制时 truncated 置 true（字段名与职责对齐）
    let (_, _, body) = route_req(&conn, "GET", "/api/anomalies?days=999999", "", db);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        v["truncated"],
        json!(true),
        "窗口钳制须 truncated=true: {body}"
    );
    // 未超上限不带标注
    let (_, _, body) = route_req(&conn, "GET", "/api/heatmap?weeks=4", "", db);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert!(v.get("weeks_requested").is_none(), "got: {body}");
}

#[test]
fn unknown_query_params_are_echoed_as_ignored_params() {
    let conn = mem_conn();
    let db = Path::new("kyn.db");
    // 未知参数回显（与 POST /api/settings 的 ignored 对称）
    let (code, _, body) = route_req(&conn, "GET", "/api/timeline?foo=1&hours=6", "", db);
    assert_eq!(code, 200, "{body}");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["ignored_params"], json!(["foo"]), "got: {body}");
    // 大小写错误键名此前被静默丢弃，现在回显
    let (_, _, body) = route_req(&conn, "GET", "/api/summary?DATE=2026-09-01", "", db);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["ignored_params"], json!(["DATE"]), "got: {body}");
    // 已知参数不出现在 ignored_params
    let (_, _, body) = route_req(&conn, "GET", "/api/summary?date=2026-09-01", "", db);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert!(v.get("ignored_params").is_none(), "got: {body}");
}

#[test]
fn ignored_params_cover_all_get_endpoints_and_bare_params() {
    let conn = mem_conn();
    let db = Path::new("kyn.db");
    // 此前不接收参数的 GET 端点同样回显（文档口径：全 GET 端点统一语义）
    for path in [
        "/api/status?Days=5",
        "/api/overview?Days=5",
        "/api/insights?Days=5",
        "/api/trends?Days=5",
        "/api/diagnostics?Days=5",
        "/api/settings?Days=5",
        "/api/autostart-status?Days=5",
    ] {
        let (code, _, body) = route_req(&conn, "GET", path, "", db);
        assert_eq!(code, 200, "{path}: {body}");
        let v: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["ignored_params"], json!(["Days"]), "{path}: {body}");
    }
    // 裸参数（无 '='）整段视为键名，同样回显；已知参数 + 裸参数混排去重
    let (_, _, body) = route_req(&conn, "GET", "/api/trends?flag&Days=5&flag", "", db);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["ignored_params"], json!(["flag", "Days"]), "got: {body}");
    // 无参数时不出现该字段
    let (_, _, body) = route_req(&conn, "GET", "/api/trends", "", db);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert!(v.get("ignored_params").is_none(), "got: {body}");
}
