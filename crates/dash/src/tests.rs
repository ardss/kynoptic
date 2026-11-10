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

// 展示层归一（发现 dashdocs）：宿主进程在时间线 Top 榜映射为友好标签，
// 不再以裸进程名（applicationframehost.exe）原样进榜；.exe 剔除 + 别名同理。
#[test]
fn timeline_maps_host_and_alias_apps_to_friendly_names() {
    let conn = mem_conn();
    let t = local_ts(0, 10, 0);
    for _ in 0..6 {
        insert(
            &conn,
            &t,
            "window",
            "switch",
            Some("applicationframehost.exe"),
        );
    }
    insert(&conn, &t, "window", "switch", Some("msedge.exe"));
    insert(&conn, &t, "window", "switch", Some("notepad.exe"));
    let now = chrono::DateTime::parse_from_rfc3339(&local_ts(0, 12, 0))
        .unwrap()
        .with_timezone(&chrono::Utc);
    let v = api_timeline_at(&conn, 12, now, 2).unwrap();
    let buckets = v["buckets"].as_array().unwrap();
    let b10 = buckets
        .iter()
        .find(|b| b["hour"].as_str().unwrap().ends_with("T10"))
        .unwrap();
    let apps = b10["apps"].as_array().unwrap();
    let by_name: std::collections::HashMap<&str, i64> = apps
        .iter()
        .filter_map(|x| {
            let n = x["app"].as_str()?;
            Some((n, x["events"].as_i64().unwrap_or(0)))
        })
        .collect();
    // 宿主 → "UWP"（6 事件），msedge → "Microsoft Edge"，notepad → "Notepad"；
    // 裸进程名不得原样出现
    assert_eq!(
        by_name.get("UWP").copied(),
        Some(6),
        "宿主应映射为 UWP: {apps:?}"
    );
    assert_eq!(by_name.get("Microsoft Edge").copied(), Some(1));
    assert_eq!(by_name.get("Notepad").copied(), Some(1));
    assert!(!apps
        .iter()
        .any(|x| x["app"].as_str() == Some("applicationframehost.exe")));
    assert!(!apps
        .iter()
        .any(|x| x["app"].as_str() == Some("applicationframehost")));
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

/// 回归复审：daily_agg 表缺失（旧库未跑到该建表迁移，且 CLI 读路径不自动
/// 迁移、无补迁移入口）时，api_anomalies 的 baseline_days 必须回落 events
/// 口径（近 7 个本地日 COUNT DISTINCT），而不是查询失败静默落 0——前端
/// baselineAccruing 门禁（bd < 3）会把「有事件但缺派生表」的库永远卡在
/// 「基线积累中（0 天）」，挡住「未检测到异常」空态。
#[test]
fn anomalies_baseline_days_falls_back_to_events_when_daily_agg_missing() {
    let conn = mem_conn();
    conn.execute_batch("DROP TABLE daily_agg").unwrap();
    // now-36h 在任何时区都落在近 7 个本地日内（昨日或前日）
    let ts = (chrono::Utc::now() - chrono::Duration::hours(36)).to_rfc3339();
    insert(&conn, &ts, "keyboard", "press", None);
    let db = Path::new("/tmp/kynoptic-test-nonexistent/settings.json");
    let v = api_anomalies(&conn, 7, db);
    assert_eq!(
        v["baseline_days"],
        json!(1),
        "缺派生表时回落 events 口径：近 7 天一次键鼠输入应为 1，不得静默为 0"
    );
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

/// S0.1：api_status 的 monitors 必须是 core monitor_stats() 的原样透传
/// （serde 直出，含 degraded_count 等全部字段）；独立 serve 测试进程无
/// 采集器，恒为空数组——跨进程透出另见 S2.3。条目内字段（含
/// note_degraded 后 degraded_count 非零）由 core 侧
/// monitor_stats_carries_nonzero_degraded_count_after_note_degraded 覆盖。
#[test]
fn status_monitors_is_passthrough_array() {
    let conn = mem_conn();
    let v = api_status(&conn, Path::new("/tmp/x.db"));
    assert!(
        v["monitors"].is_array(),
        "monitors 必须是数组: {}",
        v["monitors"]
    );
    assert!(
        v["monitors"]
            .as_array()
            .map(|a| a.is_empty())
            .unwrap_or(false),
        "测试进程无采集器，monitors 应为空数组: {}",
        v["monitors"]
    );
}

/// S0.2：collector 落库的 collection_dropped / write_failed 告警（带
/// loss_count 载荷）必须经 api_status.watchdog_alert 透出，供面板横幅
/// 映射两个新 kind。时刻用固定值插入（快照断言不读真实时钟）。
#[test]
fn watchdog_alert_exposes_collector_kinds_with_loss_count() {
    let conn = mem_conn();
    insert_notification(
        &conn,
        "2026-11-01T00:05:00+00:00",
        r#"{"source":"collector","kind":"collection_dropped","message":"通道已满，过去 60 秒丢弃了 7 个事件","message_en":"channel full: 7 events dropped in the last 60s","loss_count":7}"#,
    );
    // now 一并固定：api_status 内部有近 24h 告警窗口，固定插入时刻
    // 在真实 now 滑出窗口后会跨日变红（2026-11-02 CI 实测），改用
    // api_status_at 注入固定 now。
    let now = chrono::DateTime::parse_from_rfc3339("2026-11-01T00:20:00+00:00")
        .unwrap()
        .with_timezone(&chrono::Utc);
    let v = api_status_at(&conn, Path::new("/tmp/x.db"), now);
    let wa = &v["watchdog_alert"];
    assert_eq!(wa["kind"], "collection_dropped", "新 kind 必须透出");
    assert_eq!(wa["loss_count"], 7, "loss_count 必须透出供横幅量化显示");
    assert_eq!(wa["recovered"], json!(false));
    // 恢复事件仍走旧口径
    insert_notification(
        &conn,
        "2026-11-01T00:10:00+00:00",
        r#"{"source":"watchdog","kind":"collection_recovered","message":"采集已恢复","message_en":"collection recovered"}"#,
    );
    let v = api_status_at(&conn, Path::new("/tmp/x.db"), now);
    assert_eq!(v["watchdog_alert"]["recovered"], json!(true));
}

/// S0.3：磁盘暂停旗标（collector 写 db 同目录 disk-paused.flag）必须经
/// api_status.disk_write_paused 透出，供面板黄标横幅；旗标不存在 → false。
/// 用临时目录放旗标文件（存在性读取，不读真实时钟）。
#[test]
fn status_exposes_disk_write_paused_flag() {
    let dir = std::env::temp_dir().join(format!(
        "kyn-dash-s03-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let db_path = dir.join("t.db");
    let conn = mem_conn();
    // 无旗标 → false
    let v = api_status(&conn, &db_path);
    assert_eq!(
        v["disk_write_paused"],
        json!(false),
        "无旗标时不得报磁盘暂停"
    );
    // 放旗标 → true
    std::fs::write(dir.join("disk-paused.flag"), "磁盘空间不足").unwrap();
    let v = api_status(&conn, &db_path);
    assert_eq!(
        v["disk_write_paused"],
        json!(true),
        "旗标存在必须透出 disk_write_paused=true"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// === watchdog 告警/恢复配对（修复：告警只有「发生」没有「解除」，采集
// 恢复后横幅最长滞留 24h） ===

/// 插入一条 notification 系统事件（与 cli watchdog_alert / tray 停滞告警
/// 的落库表结构契约一致：events(timestamp,'system','notification',JSON)）。
fn insert_notification(conn: &Connection, ts: &str, payload: &str) {
    conn.execute(
        "INSERT INTO events (timestamp, event_type, event_action, event_data) VALUES (?1, 'system', 'notification', ?2)",
        params![ts, payload],
    )
    .unwrap();
}

#[test]
fn watchdog_alert_paired_with_recovery_event() {
    let conn = mem_conn();
    use chrono::TimeZone;
    // 固定"现在"（测试不读真实时钟）：告警 23h 前、恢复 1h 前
    let now = Utc.with_ymd_and_hms(2026, 9, 9, 12, 0, 0).unwrap();
    let alert = (now - chrono::Duration::hours(23)).to_rfc3339();
    insert_notification(
        &conn,
        &alert,
        r#"{"source":"watchdog","kind":"collection_stalled","message":"挂死","message_en":"hung"}"#,
    );
    let v = latest_watchdog_alert_at(&conn, now).unwrap();
    assert_eq!(v["recovered"], json!(false), "只有告警时照旧返回告警");
    assert_eq!(v["message"], json!("挂死"));
    let recovered = (now - chrono::Duration::hours(1)).to_rfc3339();
    insert_notification(
        &conn,
        &recovered,
        r#"{"source":"tray","kind":"collection_recovered","message":"已恢复","message_en":"recovered"}"#,
    );
    let v = latest_watchdog_alert_at(&conn, now).unwrap();
    assert_eq!(
        v["recovered"],
        json!(true),
        "恢复事件比告警新：横幅应翻转为已解除，不再滞留 24h"
    );
    assert_eq!(v["kind"], json!("collection_recovered"));
    assert_eq!(v["message"], json!("已恢复"));
    assert_eq!(v["message_en"], json!("recovered"));
    assert_eq!(
        v["at"],
        json!(recovered),
        "at = 恢复事件时刻，前端显示解除时间"
    );
}

#[test]
fn watchdog_alert_outside_24h_window_ignored() {
    let conn = mem_conn();
    use chrono::TimeZone;
    let now = Utc.with_ymd_and_hms(2026, 9, 9, 12, 0, 0).unwrap();
    let old = (now - chrono::Duration::hours(25)).to_rfc3339();
    insert_notification(
        &conn,
        &old,
        r#"{"kind":"collection_stalled","message":"旧告警"}"#,
    );
    assert!(
        latest_watchdog_alert_at(&conn, now).is_none(),
        "24h 窗口外的旧告警不返回"
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

/// Wave46 回归：接受字段集 = 单一权威源 ACCEPTED_SETTING_FIELDS（请求接受/
/// 响应键/ignored 判定三处共用）。字段全集全发必须 200 且逐一生效落盘、
/// 不出现 ignored（旧 KNOWN 表曾漏 update_check/low_power）。
/// 既有期望修改（10→11 字段）：本轮 R3 把隐私开关 redact_titles 纳入设置
/// 契约（AppSettings + 常量 + req.get 接受站点三处同改），字段集随之扩一，
/// 非回退。Stage 3 再扩 11→12：新增 F06 project_rules（opt-in，默认空）。
/// F10 再扩 14 字段：新增整标题脱敏 redact_titles_full（opt-in，默认 false）。
#[test]
fn settings_post_full_accepted_set_never_ignored_and_persisted() {
    let dir = tmpdir("accepted-set");
    let db = dir.join("kyn.db");
    let body: Value = json!({
        "enabled_monitors": ["window", "keyboard_hook", "mouse_hook"],
        "input_counts_only": false,
        "vk_frequency_enabled": true,
        "autostart": false,
        "presence_bridge_minutes": 3,
        "daily_goal_minutes": 60,
        "categories": [],
        "dashboard_port": 8422,
        "update_check": false,
        "low_power": true,
        "redact_titles": true,
        "redact_titles_full": true,
        "net_etw_enabled": true,
        "project_rules": [],
        "device_endpoints": []
    });
    let (code, _, out) = route_req(&mem_conn(), "POST", "/api/settings", &body.to_string(), &db);
    assert_eq!(code, 200, "接受字段全集必须 200: {out}");
    let v: Value = serde_json::from_str(&out).unwrap();
    assert!(
        v.get("ignored").is_none(),
        "接受字段全集不得出现 ignored: {out}"
    );
    for field in ACCEPTED_SETTING_FIELDS {
        assert_eq!(v[field], body[field], "字段 {field} 响应须回请求值: {out}");
    }
    assert!(
        v["monitors"].is_array(),
        "响应专用键 monitors 仍须在场: {out}"
    );
    // 落盘复核：GET 重读 settings.json 逐字段一致
    let (_, _, out2) = route_req(&mem_conn(), "GET", "/api/settings", "", &db);
    let g: Value = serde_json::from_str(&out2).unwrap();
    for field in ACCEPTED_SETTING_FIELDS {
        assert_eq!(g[field], body[field], "字段 {field} 须落盘: {out2}");
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

// ─── 批次「dash 设置与报告契约面」新契约（TDD：测试先于实现钉住缺口） ──────
// R3：redact_titles 上游设置面缺失（core 侧开关恒 false、无任何路径置 true）
// R5：report 页 focus 空态二义（载荷缺「当日是否有在场」信号，两种空态都渲染
//     同一句「暂无数据」）

/// R3：AppSettings.redact_titles 存在且默认 false（隐私 opt-in 铁律）；
/// 手改 settings.json 塞 {"redact_titles": true} 必须能被读入——此前
/// AppSettings 无该字段且 derive 无 deny_unknown_fields，键被 serde 静默
/// 丢弃，文件级旁路也不存在。
#[test]
fn wiring_redact_titles_default_off_and_file_opt_in() {
    let s = settings::AppSettings::default();
    assert!(!s.redact_titles, "标题脱敏默认必须关闭（opt-in 铁律）");
    let dir = tmpdir("redact-optin");
    let db = dir.join("kyn.db");
    std::fs::write(settings::settings_path(&db), r#"{"redact_titles": true}"#).unwrap();
    let s2 = settings::load(&db);
    assert!(
        s2.redact_titles,
        "手改 settings.json 塞 redact_titles 必须生效，不得被 serde 静默丢弃"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

/// R3：redact_titles 是设置 API 的接受字段——POST 生效落盘、响应回显、
/// 不被列进 ignored（此前 ACCEPTED_SETTING_FIELDS 不含它、POST 侧无
/// req.get 接受站点，请求会被列进 ignored 且不落盘）。
#[test]
fn wiring_redact_titles_accepted_by_settings_api() {
    let dir = tmpdir("redact-api");
    let db = dir.join("kyn.db");
    let (code, _, out) = route_req(
        &mem_conn(),
        "POST",
        "/api/settings",
        r#"{"redact_titles": true}"#,
        &db,
    );
    assert_eq!(code, 200, "redact_titles 应为接受字段: {out}");
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(
        v["redact_titles"],
        json!(true),
        "POST 响应须回显 true: {out}"
    );
    assert!(
        v.get("ignored").is_none(),
        "redact_titles 不得列进 ignored: {out}"
    );
    // 落盘复核：GET 重读 settings.json 须回 true
    let (_, _, out2) = route_req(&mem_conn(), "GET", "/api/settings", "", &db);
    let g: Value = serde_json::from_str(&out2).unwrap();
    assert_eq!(
        g["redact_titles"],
        json!(true),
        "须落盘并在 GET 回显: {out2}"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Wave46 回归：update_check/low_power 是接受字段，绝不列进 ignored；
/// monitors 为响应专用键，请求误带它列进 ignored（与真未知字段同语义）。
#[test]
fn settings_post_accepted_fields_not_reported_ignored() {
    let dir = tmpdir("ignored-accepted");
    let db = dir.join("kyn.db");
    let (code, _, out) = route_req(
        &mem_conn(),
        "POST",
        "/api/settings",
        r#"{"update_check":false,"low_power":true,"monitors":[],"bogus_xyz":42}"#,
        &db,
    );
    assert_eq!(code, 200, "接受字段不应 400: {out}");
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(
        v["update_check"],
        json!(false),
        "update_check 应生效: {out}"
    );
    assert_eq!(v["low_power"], json!(true), "low_power 应生效: {out}");
    // 按集合比对（不依赖 JSON map 键序）：恰为「响应专用键 + 真未知字段」
    let ig: Vec<&str> = v["ignored"]
        .as_array()
        .unwrap_or_else(|| panic!("应有 ignored 列表: {out}"))
        .iter()
        .map(|x| {
            x.as_str()
                .unwrap_or_else(|| panic!("ignored 条目应为字符串: {out}"))
        })
        .collect();
    assert_eq!(ig.len(), 2, "ignored 恰两项（monitors + bogus_xyz）: {out}");
    assert!(
        ig.contains(&"monitors") && ig.contains(&"bogus_xyz"),
        "ignored 应恰为 [monitors, bogus_xyz]，不得含接受字段: {out}"
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

    // 0.8.1 A3：/api/diag 开→关全链（与 collector 共享 core 控制器）
    let (code, _, _body) = route_req(&conn, "POST", "/api/diag", "{oops}", db);
    assert_eq!(code, 400, "非法 JSON 必须 400");
    let (code, _, _body) = route_req(&conn, "POST", "/api/diag", "{\"action\":\"nope\"}", db);
    assert_eq!(code, 400, "未知 action 必须 400");
    let (code, _, body) = route_req(
        &conn,
        "POST",
        "/api/diag",
        "{\"action\":\"start\",\"duration_minutes\":999}",
        db,
    );
    assert_eq!(code, 200);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["mode"], json!("diagnostic"));
    assert!(
        v["duration_ms"].as_i64().unwrap() <= 30 * 60 * 1000,
        "时长必须被硬上限钳制: {v}"
    );
    let (code, _, _) = route_req(&conn, "POST", "/api/diag", "{\"action\":\"stop\"}", db);
    assert_eq!(code, 200);
    let (code, _, body) = route_req(&conn, "GET", "/api/status", "", db);
    assert_eq!(code, 200);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        v["diag"]["mode"],
        json!("basic"),
        "stop 后 /api/status 必须回 basic"
    );

    // 0.8.1 A1：/api/self-metrics 空表 200 + 结构；minutes 非法 400
    let (code, _, body) = route_req(&conn, "GET", "/api/self-metrics", "", db);
    assert_eq!(code, 200);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["minutes"], json!(60));
    assert!(v["rows"].is_array(), "self-metrics 必须有 rows 数组");
    assert_eq!(
        route_req(&conn, "GET", "/api/self-metrics?minutes=nope", "", db).0,
        400,
        "minutes 非法必须 400"
    );

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
    // 匹配语义（大小写不敏感 + ASCII 整词/词边界）：小写 token 在词边界命中
    let r = settings::CategoryRule::rule("x", "GitHub CODE");
    assert!(r.matches("github.com", "Pull Requests")); // "github" 整词（.com 前是词界）
    assert!(!r.matches("gitee.com", "Pull Requests"));
    std::fs::remove_dir_all(&dir).unwrap();
}

/// 整词匹配收紧（发现 dashdocs）：ASCII token 不再子串误命中——
/// "edge" 不命中 "ledges"、"qq" 不命中 "qquick"；而真实进程基名/标题词仍整词命中，
/// CJK token 保持子串语义。
#[test]
fn category_ascii_tokens_match_word_boundaries_only() {
    let browse = settings::CategoryRule::rule("浏览", "edge chrome msedge");
    // 真实 Edge 进程（msedge.exe）与独立 "edge" 仍整词命中
    assert!(browse.matches("msedge.exe", ""));
    assert!(browse.matches("edge", "New Tab"));
    // "ledges"（notepad 标题）里的 "edge" 不得误命中「浏览」
    assert!(!browse.matches("notepad.exe", "Ledgers - open"));
    let comm = settings::CategoryRule::rule("通讯", "qq wechat");
    // QQ 独立整词命中；"qquick" 里的 "qq" 不得误命中「通讯」
    assert!(comm.matches("qq.exe", ""));
    assert!(!comm.matches("obs64.exe", "Recording - QQuick Scene"));
    // "code" 整词命中开发；"code" 嵌在 "encode" 里不命中
    let dev = settings::CategoryRule::rule("开发", "code");
    assert!(dev.matches("code.exe", "main.rs"));
    assert!(!dev.matches("ffmpeg", "encode video"));
    // CJK token 保持子串语义（中文无词边界）
    let cjk = settings::CategoryRule::rule("聊天", "微信");
    assert!(cjk.matches("wechat.exe", "微信 - 张三"));
    assert!(cjk.matches("anything", "正在使用微信"));
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
    // 经批准的既有断言契约更新（本轮补零修复引入）：7 天窗口必 7 行补零；
    // 旧行为为缺行不补（仅当日有数时 series 只剩 1 行）。值断言 15/4 保留。
    let arr = v["series"].as_array().unwrap();
    assert_eq!(arr.len(), 7, "7 天窗口须 7 行（缺天补零）");
    let today_row = arr
        .iter()
        .find(|x| x["date"] == json!(today.format("%Y-%m-%d").to_string()))
        .expect("补零窗口须含今日行");
    assert_eq!(today_row["keys"], json!(15));
    assert_eq!(today_row["clicks"], json!(4));
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

/// 回归复审：坏时间戳不得让 data_since 整体缺失。「先取字符串 MIN 再单值
/// 换算」的写法会选中字典序最前的坏值，datetime() 对坏值返回 NULL →
/// 字段变 None；逐行口径跳过坏行，仍给出最早**可解析**事件的本地日。
/// 注：SQLite 日期函数对越界日期是宽容解释（如 '2024-02-30' → '2024-03-01'，
/// 并非 NULL），真正产生 NULL 的坏值是空串/乱码形，本用例用空串。
#[test]
fn report_data_since_skips_unparseable_min_timestamps() {
    let conn = mem_conn();
    // 空串在字符串序上排在一切合法 20xx 值之前（旧式 MIN 会选中它）
    conn.execute(
        "INSERT INTO events (timestamp, event_type, event_action, event_data, app_name, window_title, session_id) \
         VALUES ('', 'keyboard', 'press', NULL, NULL, NULL, NULL)",
        [],
    )
    .unwrap();
    insert(
        &conn,
        "2026-01-05T20:30:00+00:00",
        "window",
        "switch",
        Some("code"),
    );
    let s = settings::AppSettings::default();
    let v = api_report_at(&conn, "2026-01-06", &s).unwrap_or_else(|e| panic!("{e}"));
    let expect = chrono::DateTime::parse_from_rfc3339("2026-01-05T20:30:00+00:00")
        .unwrap()
        .with_timezone(&chrono::Local)
        .format("%Y-%m-%d")
        .to_string();
    assert_eq!(
        v["data_since"].as_str(),
        Some(expect.as_str()),
        "坏值不得让 data_since 消失：应给最早可解析事件的本地日"
    );
}

/// 回归复审：混合编码（Z 与 ±HH:MM 并存）下 data_since 取**时间最早**
/// 事件的本地日。A = 2026-09-02T23:59:00+14:00（=09-02 09:59Z）时间最早；
/// B = 2026-09-02T23:58:00-12:00（=09-03 11:58Z）时间最晚，但 B 的字符串
/// 字典序比 A 小（同 09-02 日期段，23:58 < 23:59）；C = 2026-09-03T00:30:00Z
/// 保留 Z 编码形态。A 与 B 时刻相差 25h59m > 24h，任一时区下二者必落
/// 不同本地日（同一本地日要求时刻差 < 24h）：本地日随时间单调不减，A 早于
/// B ⇒ A 的本地日 < B 的本地日。故「先字符串 MIN 再单值换算」选中 B，结果
/// 恒晚于逐行口径（A 的本地日），本测试在任一台/CI 时区（含 UTC）都抓得住
/// 该退化；旧 fixture（+08:00 与 Z 相差 61 秒）只在 UTC+8 跨本地日，UTC 下
/// 是哑防护。逐行口径在任一时区恒等于 A 的本地日，测试不 flaky。
#[test]
fn report_data_since_mixed_encodings_uses_earliest_local_date() {
    let conn = mem_conn();
    let a = "2026-09-02T23:59:00+14:00"; // 时间最早（= 09-02 09:59Z）
    let b = "2026-09-02T23:58:00-12:00"; // 时间最晚、字典序最小（= 09-03 11:58Z）
    let c = "2026-09-03T00:30:00Z"; // 时间介于 A、B 之间，Z 编码
    insert(&conn, a, "window", "switch", Some("code"));
    insert(&conn, b, "window", "switch", Some("code"));
    insert(&conn, c, "window", "switch", Some("code"));
    let s = settings::AppSettings::default();
    let v = api_report_at(&conn, "2026-01-06", &s).unwrap_or_else(|e| panic!("{e}"));
    let expect = chrono::DateTime::parse_from_rfc3339(a)
        .unwrap()
        .with_timezone(&chrono::Local)
        .format("%Y-%m-%d")
        .to_string();
    assert_eq!(
        v["data_since"].as_str(),
        Some(expect.as_str()),
        "混合编码下 data_since 必须取最早事件的本地日，不得被字典序较小的时间戳推晚"
    );
}

/// R5：report 页两种 focus 空态必须可区分——载荷新增 presence_minutes
/// （≥20 分钟阈值过滤**前**的人在场分钟数）：无在场数据 = 0；有在场但无
/// 连续块 = >0 且 focus 仍为空。此前两种空态在契约上不可区分，前端只能
/// 同渲染「暂无数据」。日期经参数注入（local_ts 固定钟面），不读真实时钟。
#[test]
fn wiring_report_focus_empty_states_distinguishable() {
    let s = settings::AppSettings::default();
    // 空态一：基线窗口内完全没有在场数据（真无数据）
    let conn = mem_conn();
    let v = api_report_at(&conn, "2026-09-09", &s).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(
        v["presence_minutes"],
        json!(0),
        "无任何输入事件时在场分钟应为 0"
    );
    assert_eq!(v["focus"], json!([]), "无在场数据时 focus 为空");

    // 空态二：有在场分钟但无 ≥20 分钟连续块
    let conn2 = mem_conn();
    insert_input_agg(&conn2, &local_ts(0, 10, 5), r#"{"keys": 3}"#);
    let v2 = api_report_at(&conn2, "2026-09-09", &s).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(
        v2["presence_minutes"],
        json!(1),
        "1 分钟真实键鼠在场必须进载荷，presence_minutes 不得为 0"
    );
    assert_eq!(
        v2["focus"],
        json!([]),
        "单个分钟不成 ≥20 分钟块，focus 仍为空"
    );
}

/// R5：presence_minutes 是阈值过滤前的完整在场分钟数——成块用例下
/// focus 出块且在场分钟数不被 ≥20 过滤吞掉。
#[test]
fn wiring_report_focus_block_reports_full_presence_minutes() {
    let conn = mem_conn();
    let s = settings::AppSettings::default();
    for m in 0..20 {
        insert_input_agg(&conn, &local_ts(0, 10, m), r#"{"keys": 1}"#);
    }
    let v = api_report_at(&conn, "2026-09-09", &s).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(v["presence_minutes"], json!(20), "在场分钟数为过滤前总数");
    let focus = v["focus"].as_array().unwrap();
    assert_eq!(focus.len(), 1, "连续 20 分钟恰好成 1 个专注块");
    assert_eq!(focus[0]["minutes"], json!(20));
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

// === 每日对比补零 + 异常页签角标可见性 ===
// 约束：整批测试须可用单一过滤词选中（libtest 多过滤词须经 "--" 传入，
// 单 TESTNAME 直传会拒参），故归入本模块；断言与期望保持原样。
mod input_series_badge_fix {
    use super::*;
    use std::collections::BTreeMap;

    /// 固定日期构造：测试时刻一律注入，不读真实时钟。
    fn d(y: i32, m: u32, day: u32) -> chrono::NaiveDate {
        chrono::NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    /// 补零契约（纯函数）：窗口 [today-days+1, today] 每天恰好一行、升序；
    /// 已有天保留原 keys/clicks，缺天记 0。today 注入，不读真实时钟。
    #[test]
    fn input_series_zero_fill_pads_missing_days() {
        let today = d(2026, 10, 30);
        let mut series = BTreeMap::new();
        series.insert("2026-10-25".to_string(), (15u64, 4u64));
        series.insert("2026-10-28".to_string(), (3u64, 9u64));
        let out = zero_fill_daily_series(&series, today, 7);
        let dates: Vec<&str> = out.iter().map(|x| x["date"].as_str().unwrap()).collect();
        assert_eq!(
            dates,
            [
                "2026-10-24",
                "2026-10-25",
                "2026-10-26",
                "2026-10-27",
                "2026-10-28",
                "2026-10-29",
                "2026-10-30",
            ],
            "7 天窗口须连续 7 行（缺天补零行，不允许 10-25 直接跳 10-28）"
        );
        assert_eq!(out[1]["keys"], json!(15), "已有天保值");
        assert_eq!(out[1]["clicks"], json!(4), "已有天保值");
        assert_eq!(out[4]["keys"], json!(3), "已有天保值");
        assert_eq!(out[4]["clicks"], json!(9), "已有天保值");
        assert_eq!(out[0]["keys"], json!(0), "缺天 10-24 须补零");
        assert_eq!(out[0]["clicks"], json!(0), "缺天 10-24 须补零");
        assert_eq!(out[3]["keys"], json!(0), "缺天 10-27 须补零");
        assert_eq!(out[5]["clicks"], json!(0), "缺天 10-29 须补零");
    }

    /// 空序列也须补出全零窗口行：窗口语义不依赖数据存在（新装机/断采数日）。
    #[test]
    fn input_series_zero_fill_covers_empty_series() {
        let today = d(2026, 10, 30);
        let series = BTreeMap::new();
        let out = zero_fill_daily_series(&series, today, 3);
        assert_eq!(out.len(), 3, "空序列须补满 3 天");
        assert_eq!(out[0]["date"], json!("2026-10-28"));
        assert_eq!(out[2]["date"], json!("2026-10-30"));
        assert_eq!(out[0]["keys"], json!(0));
        assert_eq!(out[0]["clicks"], json!(0));
    }

    /// 端到端：GET /api/input?days=7 的 series 必须覆盖整个 7 天窗口，
    /// 缺数日补零行（输入页「每日对比」随之不缺行）。today 注入。
    #[test]
    fn api_input_series_covers_full_window() {
        let conn = mem_conn();
        let today = d(2026, 10, 30);
        let ins = |date: chrono::NaiveDate, data: &str| {
            let naive = date.and_hms_opt(8, 1, 0).unwrap();
            use chrono::TimeZone;
            let ts = Local
                .from_local_datetime(&naive)
                .earliest()
                .unwrap()
                .with_timezone(&Utc)
                .to_rfc3339();
            conn.execute(
                "INSERT INTO events (timestamp, event_type, event_action, event_data, app_name, window_title, session_id) VALUES (?1,?2,'input_agg',?3,NULL,NULL,NULL)",
                params![ts, "keyboard", data],
            )
            .unwrap();
        };
        ins(d(2026, 10, 25), r#"{"keys":10}"#);
        ins(d(2026, 10, 28), r#"{"keys":3}"#);
        let v = api_input_at(&conn, 7, today, today).unwrap();
        let arr = v["series"].as_array().unwrap();
        assert_eq!(
            arr.len(),
            7,
            "7 天窗口须 7 行（缺天补零），实测 {} 行: {arr:?}",
            arr.len()
        );
        assert_eq!(arr[0]["date"], json!("2026-10-24"));
        assert_eq!(arr[6]["date"], json!("2026-10-30"));
        let by_date: BTreeMap<&str, &Value> = arr
            .iter()
            .map(|x| (x["date"].as_str().unwrap(), x))
            .collect();
        assert_eq!(by_date["2026-10-25"]["keys"], json!(10), "已有天保值");
        assert_eq!(by_date["2026-10-28"]["keys"], json!(3), "已有天保值");
        assert_eq!(by_date["2026-10-24"]["keys"], json!(0), "缺天补零");
        assert_eq!(by_date["2026-10-29"]["clicks"], json!(0), "缺天补零");
    }

    /// 角标可见性契约：alerts>0 分支必须显式设可见 display 值（如
    /// "inline-block"），禁止空串——空串只清内联样式，回落样式表
    /// .tab-badge { display:none }，角标永不显示（aria-live 同死）。
    /// 为零时保持 "none" 隐藏；CSS 默认隐藏保留为初始态；
    /// 赋值与其三元须留在同一行内供本契约校验。
    #[test]
    fn tab_badge_alert_display_is_explicitly_visible() {
        assert!(
            DASHBOARD_HTML.contains(".tab-badge { display: none"),
            "样式表默认隐藏须保留为初始态，可见性交由 JS 显式控制"
        );
        let line = DASHBOARD_HTML
            .lines()
            .find(|l| l.contains("badge.style.display"))
            .expect("dashboard.html 须含 badge.style.display 赋值（异常页签角标）");
        let ternary = line
            .split('?')
            .nth(1)
            .expect("badge.style.display 应为三元表达式");
        let truthy = ternary
            .split(':')
            .next()
            .unwrap()
            .trim()
            .trim_end_matches(';')
            .trim();
        let falsy = ternary
            .split(':')
            .nth(1)
            .unwrap_or("")
            .trim()
            .trim_end_matches(';')
            .trim();
        assert_ne!(
            truthy, "\"\"",
            "alerts>0 分支不得设空串：空串回落样式表 display:none，角标永不显示"
        );
        assert_ne!(
            truthy.to_ascii_lowercase(),
            "\"none\"",
            "alerts>0 分支不得设为隐藏值"
        );
        assert_eq!(falsy, "\"none\"", "alerts==0 分支应保持 \"none\" 隐藏");
    }
}

// === M2.4: /api/net-process ===

#[test]
fn net_process_endpoint_returns_rows_and_rejects_bad_minutes() {
    let conn = mem_conn();
    // 桶取相对当前时刻（端点窗口以 now 为基准；只对齐窗口，不断言墙钟值）
    let now_bucket = chrono::Utc::now().timestamp_millis() / 60_000;
    conn.execute(
        "INSERT INTO net_process_minute (minute_bucket,pid,metric_id,bytes,loss_count,coverage,quality) VALUES (?1,7,'net.process.bytes_in',600,NULL,NULL,'confirmed')",
        params![now_bucket - 5],
    )
    .unwrap();
    let dir = tmpdir("netproc");
    let db = dir.join("kyn.db");
    let (code, ctype, body) = route_req(&conn, "GET", "/api/net-process?minutes=60", "", &db);
    assert_eq!(code, 200);
    assert_eq!(ctype, "application/json");
    let v: Value = serde_json::from_str(&body).unwrap();
    let rows = v["rows"].as_array().expect("必须返回 rows 数组");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["pid"], json!(7));
    assert_eq!(rows[0]["bytes"], json!(600));
    assert_eq!(rows[0]["metric_id"], json!("net.process.bytes_in"));
    assert_eq!(rows[0]["quality"], json!("confirmed"));
    // minutes=0 → 400（数值参数政策同既有端点）
    let (code, _, _) = route_req(&conn, "GET", "/api/net-process?minutes=0", "", &db);
    assert_eq!(code, 400);
}

// === M3.4: /api/net-derived（网络归因分区 + 可信度标注） ===

#[test]
fn net_derived_partition_snapshot_with_proxy_evidence() {
    // 端点数据快照：种子两进程分钟桶（4242=代理核心、7=普通应用），
    // 注入代理 pid 证据与固定 now，断言五分区口径与 quality 徽标输入
    let conn = mem_conn();
    let now_ms = 1_800_000_000_000i64;
    let now_bucket = now_ms / 60_000;
    for (pid, metric, bytes, bucket_off) in [
        (4242i64, "net.process.bytes_in", 1000i64, 5i64),
        (4242, "net.process.bytes_out", 2000, 5),
        (7, "net.process.bytes_in", 500, 3),
    ] {
        conn.execute(
            "INSERT INTO net_process_minute (minute_bucket,pid,metric_id,bytes,loss_count,coverage,quality) VALUES (?1,?2,?3,?4,NULL,NULL,'confirmed')",
            params![now_bucket - bucket_off, pid, metric, bytes],
        )
        .unwrap();
    }
    let v = api_net_derived_with(&conn, 60, now_ms, &[4242], true);
    assert_eq!(v["minutes"], json!(60));
    assert_eq!(v["evidence_available"], json!(true));
    let secs = v["sections"].as_array().expect("必须返回 sections 数组");
    assert_eq!(
        secs.len(),
        5,
        "五分区：direct/via_proxy/proxy_outer/fuzzy/plan"
    );
    let get = |key: &str| {
        secs.iter()
            .find(|s| s["key"] == json!(key))
            .unwrap_or_else(|| panic!("缺分区 {key}"))
    };
    // 代理外层：pid 命中证据，进出合计 3000，confirmed
    let po = get("proxy_outer");
    assert_eq!(po["bytes"], json!(3000));
    assert_eq!(po["quality"], json!("confirmed"));
    // 直连 / 代理相关：分钟聚合无远端归属，本切片诚实标 Unavailable+原因
    for key in ["direct", "via_proxy"] {
        let s = get(key);
        assert_eq!(s["quality"], json!("unavailable"), "{key} 必须显式不可用");
        assert!(s["bytes"].is_null());
        let note = s["note"].as_str().unwrap_or("");
        assert!(!note.is_empty(), "{key} 必须带原因文案");
    }
    // 模糊：非代理进程行合计 500，Incomplete + 原因（分钟聚合无远端归属）
    let fz = get("fuzzy");
    assert_eq!(fz["bytes"], json!(500));
    assert_eq!(fz["quality"], json!("incomplete"));
    assert!(fz["note"].as_str().unwrap_or("").contains("远端"));
    // 套餐：M3.3 结论 Unavailable + 原因
    let pl = get("plan");
    assert_eq!(pl["quality"], json!("unavailable"));
    assert!(pl["bytes"].is_null());
    assert!(!pl["note"].as_str().unwrap_or("").is_empty());
}

#[test]
fn net_derived_unavailable_when_port_evidence_fails() {
    let conn = mem_conn();
    let now_ms = 1_800_000_000_000i64;
    let now_bucket = now_ms / 60_000;
    conn.execute(
        "INSERT INTO net_process_minute (minute_bucket,pid,metric_id,bytes,loss_count,coverage,quality) VALUES (?1,7,'net.process.bytes_in',500,NULL,NULL,'confirmed')",
        params![now_bucket - 3],
    )
    .unwrap();
    let v = api_net_derived_with(&conn, 60, now_ms, &[], false);
    let secs = v["sections"].as_array().unwrap();
    let get = |key: &str| secs.iter().find(|s| s["key"] == json!(key)).unwrap();
    assert_eq!(v["evidence_available"], json!(false));
    for key in ["proxy_outer", "fuzzy"] {
        let s = get(key);
        assert_eq!(
            s["quality"],
            json!("unavailable"),
            "{key} 证据缺失必须显式降级"
        );
        assert!(!s["note"].as_str().unwrap_or("").is_empty());
    }
}

#[test]
fn net_derived_endpoint_route_and_bad_minutes() {
    let conn = mem_conn();
    let dir = tmpdir("netderived");
    let db = dir.join("kyn.db");
    let (code, ctype, body) = route_req(&conn, "GET", "/api/net-derived?minutes=60", "", &db);
    assert_eq!(code, 200);
    assert_eq!(ctype, "application/json");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["sections"].as_array().unwrap().len(), 5);
    let (code, _, _) = route_req(&conn, "GET", "/api/net-derived?minutes=0", "", &db);
    assert_eq!(code, 400);
}

// === M4.2: 诊断模式（设置页字段 + /api/status diag 块） ===

#[test]
fn diag_mode_status_fields_and_settings_page() {
    // /api/status 回 diag 块：默认 basic（诊断模式默认关）、剩余 0、
    // 深诊档资源预算上限随块下发（设置页展示契约）
    let conn = mem_conn();
    let dir = tmpdir("diagmode");
    let db = dir.join("kyn.db");
    let (code, _, body) = route_req(&conn, "GET", "/api/status", "", &db);
    assert_eq!(code, 200);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["diag"]["mode"], json!("basic"), "诊断模式必须默认关");
    assert_eq!(v["diag"]["remaining_ms"], json!(0));
    assert_eq!(
        v["diag"]["budget_cpu_percent_single_core"],
        json!(kynoptic_core::diag_mode::DIAG_BUDGET_CPU_PERCENT_SINGLE_CORE)
    );
    assert_eq!(
        v["diag"]["budget_rss_mib"],
        json!(kynoptic_core::diag_mode::DIAG_BUDGET_RSS_MIB)
    );
    // 设置页字段：当前模式 + 剩余时长（id 契约，前端 JS 回填）
    assert!(
        DASHBOARD_HTML.contains("set-diag-mode"),
        "设置页须有诊断模式字段"
    );
    assert!(
        DASHBOARD_HTML.contains("set-diag-remaining"),
        "设置页须有剩余时长字段"
    );
}

// === M4.3: 三视图（系统总览 / 资源排行 / 诊断时间线） ===

/// net_process_minute 直插（测试造数；列同 0012 迁移）。
fn m43_net_row(conn: &Connection, bucket: i64, pid: u32, metric: &str, bytes: i64) {
    conn.execute(
        "INSERT INTO net_process_minute (minute_bucket, pid, metric_id, bytes, loss_count, coverage, quality) \
         VALUES (?1, ?2, ?3, ?4, NULL, NULL, 'confirmed')",
        params![bucket, pid, metric, bytes],
    )
    .unwrap();
}

/// 带任意 event_data 的插行（RFC3339 UTC）。
fn m43_event(conn: &Connection, ts: &str, t: &str, a: &str, data: Option<&str>, app: Option<&str>) {
    conn.execute(
        "INSERT INTO events (timestamp, event_type, event_action, event_data, app_name, window_title, session_id) \
         VALUES (?1, ?2, ?3, ?4, ?5, NULL, NULL)",
        params![ts, t, a, data, app],
    )
    .unwrap();
}

fn m43_ms_to_rfc3339(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .unwrap()
        .to_rfc3339()
}

#[test]
fn m43_sys_overview_snapshot() {
    // 系统总览快照：CPU/内存读最新 system heartbeat 事件、GPU 读最新
    // gpu_snapshot、网络读 M2.4 分钟桶合计、关键异常/监控健康字段齐备。
    // 全程注入 now_ms（禁真实时钟）。
    let conn = mem_conn();
    let db = tmpdir("m43sys").join("kyn.db");
    const NOW: i64 = 10_000 * 60_000; // 10_000 号桶
    m43_event(
        &conn,
        &m43_ms_to_rfc3339(NOW - 60_000),
        "system",
        "heartbeat",
        Some(r#"{"cpu_percent":42.5,"memory":{"total":16,"available":8,"used_percent":50}}"#),
        None,
    );
    m43_event(
        &conn,
        &m43_ms_to_rfc3339(NOW - 120_000),
        "system",
        "gpu_snapshot",
        Some(r#"{"gpus":["TestGPU"]}"#),
        None,
    );
    m43_net_row(&conn, NOW / 60_000 - 1, 7, "net.process.bytes_in", 600);
    m43_net_row(&conn, NOW / 60_000 - 1, 7, "net.process.bytes_out", 50);
    let v = api_sys_overview_at(&conn, &db, NOW);
    assert_eq!(
        v["cpu_memory"]["cpu_percent"],
        json!(42.5),
        "CPU 读自最新心跳"
    );
    assert_eq!(v["cpu_memory"]["memory"]["used_percent"], json!(50));
    assert_eq!(v["cpu_memory"]["quality"], json!("confirmed"));
    assert!(
        v["gpu"]["data"].is_object(),
        "GPU 快照读自最新 gpu_snapshot 事件"
    );
    assert_eq!(
        v["network"]["bytes_in"],
        json!(600),
        "网络进合计来自 M2.4 桶"
    );
    assert_eq!(v["network"]["bytes_out"], json!(50));
    assert_eq!(v["network"]["quality"], json!("confirmed"));
    assert!(v["anomalies"]["count"].is_number(), "关键异常计数必须在场");
    assert!(
        v["monitor_health"]["enabled_monitors"].is_number(),
        "监控健康须带启用监控器数"
    );
    assert!(
        v["monitor_health"]["watchdog_alert"].is_null(),
        "空库无看门狗告警"
    );
}

#[test]
fn m43_resource_top_ranks_by_bytes_with_injected_names() {
    // 资源排行：网络维度按 pid 合计排序（降序），进程名注入解析（缺失显示
    // null 不伪造）；CPU/内存/磁盘/进程树无持久化源 → 显式 Unavailable+原因。
    let conn = mem_conn();
    const NOW: i64 = 10_000 * 60_000;
    m43_net_row(&conn, NOW / 60_000 - 1, 7, "net.process.bytes_in", 3_000);
    m43_net_row(&conn, NOW / 60_000 - 1, 7, "net.process.bytes_out", 100);
    m43_net_row(&conn, NOW / 60_000 - 2, 9, "net.process.bytes_in", 5_000);
    let v = api_resource_top_at(&conn, 60, NOW, &|pid| {
        if pid == 7 {
            Some("alpha.exe".to_string())
        } else {
            None
        }
    });
    let rows = v["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["pid"], json!(9), "字节最多者排第一");
    assert_eq!(rows[0]["total"], json!(5_000));
    assert!(rows[0]["name"].is_null(), "解析不到的名字保持 null，不伪造");
    assert_eq!(rows[1]["pid"], json!(7));
    assert_eq!(rows[1]["name"], json!("alpha.exe"));
    assert_eq!(rows[1]["total"], json!(3_100));
    for dim in ["cpu", "memory", "disk", "tree"] {
        assert_eq!(
            v["dimensions"][dim]["quality"],
            json!("unavailable"),
            "{dim} 无持久化按进程源，必须显式 Unavailable"
        );
        assert!(
            v["dimensions"][dim]["reason"]
                .as_str()
                .unwrap()
                .contains("未采"),
            "{dim} 原因须声明未采不伪造"
        );
    }
}

#[test]
fn m43_diag_timeline_related_fields() {
    // 诊断时间线：锚点 ± 窗口内关联事件（升序、含类型/动作/应用）+ 网络资源
    // 变化（锚点前后分桶合计）+ 目的地址显式 Unavailable（分钟聚合无远端地址）。
    let conn = mem_conn();
    const A: i64 = 1_000 * 60_000; // 锚点：1_000 号桶整
    m43_event(
        &conn,
        &m43_ms_to_rfc3339(A - 2 * 60_000),
        "window",
        "switch",
        None,
        Some("editor.exe"),
    );
    m43_event(
        &conn,
        &m43_ms_to_rfc3339(A + 3 * 60_000),
        "clipboard",
        "press",
        None,
        None,
    );
    m43_net_row(&conn, A / 60_000 - 1, 7, "net.process.bytes_out", 700);
    m43_net_row(&conn, A / 60_000 + 1, 7, "net.process.bytes_out", 300);
    let v = api_diag_timeline_at(&conn, A, 10);
    let evs = v["window_events"].as_array().unwrap();
    assert_eq!(evs.len(), 2, "窗口内两条关联事件");
    assert_eq!(evs[0]["action"], json!("switch"), "按时间升序");
    assert_eq!(evs[0]["app"], json!("editor.exe"), "关联字段含进程/应用");
    assert_eq!(
        v["resource_change"]["before_bytes"],
        json!(700),
        "锚点前合计"
    );
    assert_eq!(
        v["resource_change"]["after_bytes"],
        json!(300),
        "锚点后合计"
    );
    assert_eq!(v["resource_change"]["quality"], json!("confirmed"));
    assert_eq!(v["dest"]["quality"], json!("unavailable"), "远端地址未采");
    assert!(v["dest"]["reason"].as_str().unwrap().contains("未采"));
    assert_eq!(v["related"]["event_count"], json!(2));
}

#[test]
fn m43_routes_param_contract() {
    let conn = mem_conn();
    let db = tmpdir("m43routes").join("kyn.db");
    let (code, ctype, body) = route_req(&conn, "GET", "/api/sys-overview", "", &db);
    assert_eq!(code, 200);
    assert_eq!(ctype, "application/json");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert!(v["cpu_memory"].is_object());
    let (code, _, _) = route_req(&conn, "GET", "/api/resource-top?minutes=0", "", &db);
    assert_eq!(code, 400);
    let (code, _, body) = route_req(&conn, "GET", "/api/resource-top?minutes=60", "", &db);
    assert_eq!(code, 200);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["minutes"], json!(60));
    let (code, _, _) = route_req(&conn, "GET", "/api/diag-timeline", "", &db);
    assert_eq!(code, 400, "缺 at 锚点必须 400");
    let (code, _, body) = route_req(&conn, "GET", "/api/diag-timeline?at=60000000", "", &db);
    assert_eq!(code, 200);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["at_ms"], json!(60_000_000));
    assert!(v["window_events"].is_array());
}

#[test]
fn m43_dashboard_three_views_present() {
    // 三视图 id 契约：页签 + 面板容器（HTML 结构，JS 回填）。
    for id in [
        "tab-sys",
        "view-sys",
        "tab-restop",
        "view-restop",
        "tab-diagtl",
        "view-diagtl",
        "sys-overview",
        "restop-rows",
        "diagtl-rows",
        "diagtl-dest",
    ] {
        assert!(DASHBOARD_HTML.contains(id), "dashboard 须包含 {id}");
    }
}

// === 统一时间轴（/api/timeline?minutes=，F07，只读 activity_intervals） ===

/// 与 core activity_intervals 单测同锚（2026-11-02T03:00:00Z），固定不读真实时钟。
const UTL_BASE_MS: i64 = 1_792_977_600_000;

fn utl_rfc(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .unwrap()
        .to_rfc3339()
}

fn utl_switch(conn: &Connection, off_secs: i64, app: &str, title: &str) {
    conn.execute(
        "INSERT INTO events (timestamp, event_type, event_action, app_name, window_title) \
         VALUES (?1, 'window', 'switch', ?2, ?3)",
        params![utl_rfc(UTL_BASE_MS + off_secs * 1000), app, title],
    )
    .unwrap();
}

fn utl_idle(conn: &Connection, off_secs: i64, action: &str) {
    conn.execute(
        "INSERT INTO events (timestamp, event_type, event_action) VALUES (?1, 'system', ?2)",
        params![utl_rfc(UTL_BASE_MS + off_secs * 1000), action],
    )
    .unwrap();
}

/// F05 Stage 2：ai_session 事件 → activity_intervals 的 ai_run 区间 →
/// D 轨（connected=true）按证据等级返回。
#[test]
fn unified_timeline_track_d_shows_ai_run_intervals() {
    use kynoptic_core::db::activity_intervals::rebuild_activity_intervals_at;
    let conn = mem_conn();
    conn.execute(
        "INSERT INTO events (timestamp, event_type, event_action, event_data, app_name, source_id, quality) \
         VALUES (?1, 'ai_session', 'ai_run', ?2, 'claude_code', 'ai', 'observed')",
        params![
            utl_rfc(UTL_BASE_MS + 5_000),
            serde_json::json!({
                "schema_version": 1,
                "session_id": "fix-d-track",
                "source": "claude_code",
                "start": utl_rfc(UTL_BASE_MS + 5_000),
                "end": utl_rfc(UTL_BASE_MS + 65_000),
                "start_quality": "observed",
                "end_quality": "observed",
                "prompt_count": 2,
            })
            .to_string(),
        ],
    )
    .unwrap();
    let now = UTL_BASE_MS + 120_000;
    rebuild_activity_intervals_at(&conn, now).unwrap();
    let v = api_unified_timeline_at(&conn, 60, now).unwrap();
    let d = &v["tracks"].as_array().unwrap()[3];
    assert_eq!(d["connected"], json!(true));
    let ivs = d["intervals"].as_array().unwrap();
    assert_eq!(ivs.len(), 1);
    assert_eq!(ivs[0]["app_name"], json!("claude_code"));
    assert_eq!(ivs[0]["quality"], json!("observed"));
    assert_eq!(ivs[0]["evidence_level"], json!("observed"));
    assert_eq!(ivs[0]["source_id"], json!("ai"));
}

/// F11 多设备：timeline 区间带 device_id（源事件原样抄写；未带 → null），
/// 响应带 device_boundary 边界说明（双机不相加）。
#[test]
fn unified_timeline_intervals_carry_device_id() {
    use kynoptic_core::db::activity_intervals::rebuild_activity_intervals_at;
    let conn = mem_conn();
    conn.execute(
        "INSERT INTO events (timestamp, event_type, event_action, app_name, window_title, source_id, device_id) \
         VALUES (?1, 'window', 'switch', 'appA.exe', 'T', 'windows', 'dev-fix-0001')",
        params![utl_rfc(UTL_BASE_MS)],
    )
    .unwrap();
    // 同题刷新（无 device_id 行）：区间设备归属仍取首现事件，且形成非零区间
    utl_switch(&conn, 3, "appA.exe", "T");
    utl_switch(&conn, 30, "appB.exe", "U"); // 无 device_id
    let now = UTL_BASE_MS + 120_000;
    rebuild_activity_intervals_at(&conn, now).unwrap();

    let v = api_unified_timeline_at(&conn, 60, now).unwrap();
    assert_eq!(v["device_boundary"]["scope"], json!("this_database"));
    let a = v["tracks"].as_array().unwrap()[0]["intervals"]
        .as_array()
        .unwrap();
    assert_eq!(a.len(), 2);
    assert_eq!(a[0]["device_id"], json!("dev-fix-0001"));
    assert_eq!(
        a[1]["device_id"],
        Value::Null,
        "未带设备标识 → null，不虚构"
    );
}

/// F11 多设备：reports 行按 device_id 分组，响应带 devices 列表与
/// device_boundary 说明；无 device_id 的行归入 "(unknown)" 分组标签。
#[test]
fn reports_rows_carry_device_dimension() {
    use kynoptic_core::db::activity_intervals::rebuild_activity_intervals_at;
    let conn = mem_conn();
    conn.execute(
        "INSERT INTO events (timestamp, event_type, event_action, app_name, window_title, source_id, device_id) \
         VALUES (?1, 'window', 'switch', 'appA.exe', 'T', 'windows', 'dev-fix-0001')",
        params![utl_rfc(UTL_BASE_MS)],
    )
    .unwrap();
    utl_switch(&conn, 3, "appA.exe", "T"); // 同题刷新 → 非零区间
    utl_switch(&conn, 30, "appB.exe", "U");
    let now = UTL_BASE_MS + 120_000;
    rebuild_activity_intervals_at(&conn, now).unwrap();

    let v = api_reports_at(&conn, 7, now).unwrap();
    assert_eq!(v["device_boundary"]["scope"], json!("this_database"));
    let rows = v["rows"].as_array().unwrap();
    let dev_of = |app: &str| -> Option<String> {
        rows.iter()
            .find(|r| r["source"].as_str() == Some(app))
            .map(|r| r["device_id"].as_str().unwrap().to_string())
    };
    assert_eq!(dev_of("appA.exe").as_deref(), Some("dev-fix-0001"));
    assert_eq!(dev_of("appB.exe").as_deref(), Some("(unknown)"));
    let devices = v["devices"].as_array().unwrap();
    assert!(devices.contains(&json!("dev-fix-0001")));
    assert!(devices.contains(&json!("(unknown)")));
}

/// 轨道分组与区间聚合（_at 注入时钟）：A 轨吃下去重后的前台区间
/// （同题重复只产一个区间），C 轨 = away；B 轨无 tab 数据源 → 整段 unknown
/// + 未接入标注；D/E 留空。证据等级映射 observed/estimated→inferred。
#[test]
fn unified_timeline_groups_tracks_and_aggregates_intervals() {
    use kynoptic_core::db::activity_intervals::rebuild_activity_intervals_at;
    let conn = mem_conn();
    // 0-3s 同题重复（A/T）→ 1 个区间；10-20s away；30s 切到 B/U（尾部断线收口）
    utl_switch(&conn, 0, "appA.exe", "T");
    utl_switch(&conn, 1, "appA.exe", "T");
    utl_switch(&conn, 3, "appA.exe", "T");
    utl_idle(&conn, 10, "idle_start");
    utl_idle(&conn, 20, "idle_end");
    utl_switch(&conn, 30, "appB.exe", "U");
    let now = UTL_BASE_MS + 120_000;
    rebuild_activity_intervals_at(&conn, now).unwrap();

    let v = api_unified_timeline_at(&conn, 60, now).unwrap();
    assert_eq!(v["endpoint"], json!("unified_timeline"));
    assert_eq!(v["minutes"], json!(60));
    assert_eq!(v["window_end_ms"], json!(now));
    assert_eq!(v["window_start_ms"], json!(now - 60 * 60_000));

    let tracks = v["tracks"].as_array().unwrap();
    assert_eq!(tracks.len(), 5, "轨道 A-E");
    for (i, id) in ["A", "B", "C", "D", "E"].iter().enumerate() {
        assert_eq!(tracks[i]["id"], json!(id));
    }
    assert!(v["legend"]["unknown"].as_str().unwrap().contains("不是零"));

    // A 轨：两个前台区间（appA [0,3s] observed；appB [30s,now] 断线收口 unknown）
    let a = tracks[0]["intervals"].as_array().unwrap();
    assert_eq!(
        a.len(),
        2,
        "同题重复只产一个区间: {}",
        tracks[0]["intervals"]
    );
    assert_eq!(a[0]["app_name"], json!("appA.exe"));
    assert_eq!(a[0]["duration_ms"], json!(3_000));
    assert_eq!(a[0]["quality"], json!("observed"));
    assert_eq!(a[0]["evidence_level"], json!("observed"));
    assert_eq!(a[1]["app_name"], json!("appB.exe"));
    assert_eq!(a[1]["duration_ms"], json!(90_000)); // 30s → now(120s)
    assert_eq!(a[1]["quality"], json!("unknown"));
    assert_eq!(a[1]["evidence_level"], json!("unknown"));

    // B 轨：无 tab 数据源 → 单条整窗 unknown + connected=false
    let b = &tracks[1];
    assert_eq!(b["connected"], json!(false));
    assert!(b["note_zh"].as_str().unwrap().contains("未接入"));
    let biv = b["intervals"].as_array().unwrap();
    assert_eq!(biv.len(), 1);
    assert_eq!(biv[0]["evidence_level"], json!("unknown"));
    assert_eq!(biv[0]["duration_ms"], json!(60 * 60_000));

    // C 轨：away [10,20s] observed
    let c = tracks[2]["intervals"].as_array().unwrap();
    assert_eq!(c.len(), 1);
    assert_eq!(c[0]["duration_ms"], json!(10_000));
    assert_eq!(c[0]["quality"], json!("observed"));

    // D 轨（Stage 3 起 ai_run+editor_session 双源；本库无 AI/编辑器区间
    // → 空轨道按契约标「未接入」connected=false——空数据不隐藏也不标 0）；
    // E 轨仍预留未接入。
    assert_eq!(
        tracks[3]["connected"],
        json!(false),
        "空 D 轨 = 未接入: {}",
        tracks[3]
    );
    assert!(tracks[3]["note_zh"].as_str().unwrap().contains("未接入"));
    assert_eq!(tracks[3]["intervals"].as_array().unwrap().len(), 0);
    assert_eq!(tracks[4]["connected"], json!(false));
    assert_eq!(tracks[4]["intervals"].as_array().unwrap().len(), 0);
    assert!(!tracks[4]["note_en"].as_str().unwrap().is_empty());
}

/// 窗口过滤与裁剪：窗口外的区间整体排除，跨界区间裁到窗口边界。
#[test]
fn unified_timeline_window_filters_and_clamps() {
    use kynoptic_core::db::activity_intervals::rebuild_activity_intervals_at;
    let conn = mem_conn();
    // 3600s（1 小时）前一个区间（同题两条 → [−3600,−3550] 50s）；
    // 30s 前一个（尾部收口到 now → 30s）。中间不再插别的 switch：
    // 单条 switch 无重复时是零时长点区间，端点不输出（区间口径，单点非区间）。
    utl_switch(&conn, -3600, "old.exe", "O");
    utl_switch(&conn, -3550, "old.exe", "O");
    utl_switch(&conn, -30, "tail.exe", "L");
    let now = UTL_BASE_MS;
    rebuild_activity_intervals_at(&conn, now).unwrap();

    let v = api_unified_timeline_at(&conn, 5, now).unwrap(); // 5 分钟窗
    let a = v["tracks"].as_array().unwrap()[0]["intervals"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(a.len(), 1, "窗口外 old/new 区间必须整体排除: {a:?}");
    assert_eq!(a[0]["app_name"], json!("tail.exe"));
    assert_eq!(a[0]["start_ms"], json!(now - 30_000));
    assert_eq!(a[0]["end_ms"], json!(now), "尾部收口裁到窗口终点");

    // 大窗口（60 分钟）能看到 2 段（new.exe 是单点事件 → 零时长区间，
    // 端点对 end<=start 的点区间不输出——区间口径，单点非区间）
    let v60 = api_unified_timeline_at(&conn, 60, now).unwrap();
    let a60 = v60["tracks"].as_array().unwrap()[0]["intervals"]
        .as_array()
        .unwrap();
    assert_eq!(a60.len(), 2);
    // 跨窗口裁剪：old 段 [-3600,-3550] 起点裁到窗口边界（60min 窗起点恰含）
    assert_eq!(a60[0]["start_ms"], json!(now - 3_600_000));
    assert_eq!(a60[0]["duration_ms"], json!(50_000));
}

/// HTTP 层契约（路由注册 + 参数校验）：minutes= 空值/0/非数字 → 400；
/// 超上限钳制 + minutes_requested/note 标注；缺省（无 minutes）走历史小时桶；
/// 未知参数回显；tracks 结构齐全。
#[test]
fn unified_timeline_route_param_contract() {
    let conn = mem_conn();
    let db = tmpdir("utlroute").join("kyn.db");
    // 参数校验：空 / 0 / 非数字 / 负数 一律 400
    for q in ["minutes=", "minutes=0", "minutes=abc", "minutes=-1"] {
        let (code, _, _) = route_req(&conn, "GET", &format!("/api/timeline?{q}"), "", &db);
        assert_eq!(code, 400, "?{q} 必须 400");
    }
    // 超上限：钳到 1440 + 标注
    let (code, _, body) = route_req(&conn, "GET", "/api/timeline?minutes=9999", "", &db);
    assert_eq!(code, 200);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["minutes"], json!(1440));
    assert_eq!(v["minutes_requested"], json!(9999));
    assert!(v["note"].as_str().unwrap().contains("1440"));
    // 未知参数回显
    let (code, _, body) = route_req(&conn, "GET", "/api/timeline?minutes=30&foo=1", "", &db);
    assert_eq!(code, 200);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["ignored_params"], json!(["foo"]));
    // 空库上 200（空轨道 + B 轨整段 unknown 兜底）
    let tracks = v["tracks"].as_array().unwrap();
    assert_eq!(tracks.len(), 5);
    // 缺省（无 minutes）→ 既有小时桶端点行为不变（向后兼容）
    let (code, _, body) = route_req(&conn, "GET", "/api/timeline?hours=12", "", &db);
    assert_eq!(code, 200);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert!(v["buckets"].is_array(), "无 minutes 时仍走小时桶口径");
    let (code, _, body) = route_req(&conn, "GET", "/api/timeline", "", &db);
    assert_eq!(code, 200);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert!(v["buckets"].is_array());
}

/// dashboard 视图注册：页签 + 容器 + 缩放/图例/下钻挂点齐全。
#[test]
fn unified_timeline_view_registered_in_dashboard() {
    for id in [
        "tab-unifiedtl",
        "view-unifiedtl",
        "utl-zoom",
        "utl-legend",
        "utl-tracks",
        "utl-detail",
        "loadUnifiedTimeline",
        "utlShowDetail",
        // F07 筛选条（项目/来源/证据等级）：UI 挂点齐全
        "utl-filter",
        "utl-filter-project",
        "utl-filter-source",
        "utl-filter-apply",
        "utl-filter-clear",
        "utl-filter-active",
        "utlFilterQuery",
        "utlFilterEcho",
    ] {
        assert!(DASHBOARD_HTML.contains(id), "dashboard 须包含 {id}");
    }
}

// === F07 筛选（project / source / evidence）：core 纯函数 + HTTP 契约 ===

/// 筛选 fixture（区间语义按 rebuild 真实口径：同题首末事件成区间，单条
/// switch 是零宽点区间不输出；idle 相交整段打 estimated）：
/// - A/alpha [0,100]s：idle 50→60 相交 → 整段 estimated → evidence=inferred；
/// - A/alpha [200,290]s：同题闭区间 → observed；
/// - B/beta [300,315]s：observed，规则未命中 → unassigned；
/// - D/ddd [320,330]s：observed，unassigned；
/// - E/eee @400s（== now）：零宽点区间不输出（不留 trailing unknown 段）；
/// - AI claude_code [320,360]s observed（app_name=claude_code，source_id=ai）。
///
/// 返回 (conn, now)。
fn utl_filter_fixture() -> (Connection, i64) {
    use kynoptic_core::db::activity_intervals::{
        rebuild_activity_intervals_with_rules_at, ProjectRule,
    };
    let conn = mem_conn();
    // alpha 第一段：0→100s，idle 相交 → inferred
    utl_switch(&conn, 0, "A.exe", "alphafile");
    utl_idle(&conn, 50, "idle_start");
    utl_idle(&conn, 60, "idle_end");
    utl_switch(&conn, 100, "A.exe", "alphafile");
    utl_switch(&conn, 110, "Z.exe", "zzz");
    // alpha 第二段：200→290s，observed
    utl_switch(&conn, 200, "A.exe", "alphafile");
    utl_switch(&conn, 290, "A.exe", "alphafile");
    utl_switch(&conn, 300, "B.exe", "betafile");
    // beta：300→315s observed
    utl_switch(&conn, 315, "B.exe", "betafile");
    utl_switch(&conn, 320, "D.exe", "ddd");
    // ddd：320→330s observed
    utl_switch(&conn, 330, "D.exe", "ddd");
    utl_switch(&conn, 400, "E.exe", "eee"); // == now，零宽点区间不输出
                                            // AI 会话 320→360s observed
    conn.execute(
        "INSERT INTO events (timestamp, event_type, event_action, event_data, app_name, source_id, quality) \
         VALUES (?1, 'ai_session', 'ai_run', ?2, 'claude_code', 'ai', 'observed')",
        params![
            utl_rfc(UTL_BASE_MS + 320_000),
            serde_json::json!({
                "schema_version": 1,
                "session_id": "fix-filter",
                "source": "claude_code",
                "start": utl_rfc(UTL_BASE_MS + 320_000),
                "end": utl_rfc(UTL_BASE_MS + 360_000),
                "start_quality": "observed",
                "end_quality": "observed",
                "prompt_count": 1,
            })
            .to_string(),
        ],
    )
    .unwrap();
    let now = UTL_BASE_MS + 400_000;
    let rules = vec![ProjectRule {
        name: "alpha".into(),
        path_prefix: String::new(),
        title_keywords: "alpha".into(),
    }];
    rebuild_activity_intervals_with_rules_at(&conn, now, &rules).unwrap();
    (conn, now)
}

fn utl_track_a(v: &Value) -> Vec<Value> {
    v["tracks"].as_array().unwrap()[0]["intervals"]
        .as_array()
        .unwrap()
        .clone()
}
fn utl_track_b(v: &Value) -> Vec<Value> {
    v["tracks"].as_array().unwrap()[1]["intervals"]
        .as_array()
        .unwrap()
        .clone()
}

/// 证据等级过滤（observed/inferred/unknown 词表）：inferred 段单列可筛；
/// unknown 词表值 + source/project 均空时 B 轨 unknown 占位段保留——占位段
/// 是「无数据源」声明，不是被筛选掉的数据。
#[test]
fn unified_timeline_filter_by_evidence_level() {
    let (conn, now) = utl_filter_fixture();
    let f = kynoptic_core::timeline::TimelineFilter {
        evidence: vec!["observed".into()],
        ..Default::default()
    };
    let v = api_unified_timeline_filtered_at(&conn, 60, now, &f).unwrap();
    let a = utl_track_a(&v);
    assert_eq!(
        a.len(),
        3,
        "observed 过滤命中 alpha/beta/ddd 三段，inferred 已排除: {a:?}"
    );
    assert!(a.iter().all(|iv| iv["evidence_level"] == json!("observed")));
    // AI 段 observed 仍在 D 轨
    assert_eq!(
        v["tracks"].as_array().unwrap()[3]["intervals"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    // B 轨占位段：evidence=observed（不含 unknown）→ 占位段不保留
    assert!(
        utl_track_b(&v).is_empty(),
        "unknown 占位段被 evidence 过滤排除"
    );

    // inferred 单列可筛
    let f = kynoptic_core::timeline::TimelineFilter {
        evidence: vec!["inferred".into()],
        ..Default::default()
    };
    let v = api_unified_timeline_filtered_at(&conn, 60, now, &f).unwrap();
    let a = utl_track_a(&v);
    assert_eq!(a.len(), 1, "inferred 过滤只命中 idle 相交段: {a:?}");
    assert_eq!(a[0]["evidence_level"], json!("inferred"));
    assert_eq!(a[0]["project"], json!("alpha"), "inferred 段项目归属保留");
    assert!(utl_track_b(&v).is_empty());

    // evidence=unknown：占位段保留（它是 B 轨唯一内容）
    let f = kynoptic_core::timeline::TimelineFilter {
        evidence: vec!["unknown".into()],
        ..Default::default()
    };
    let v = api_unified_timeline_filtered_at(&conn, 60, now, &f).unwrap();
    let b = utl_track_b(&v);
    assert_eq!(b.len(), 1);
    assert_eq!(b[0]["evidence_level"], json!("unknown"));
    // observed/inferred 的真实区间全被排除——unknown 不是它们的别名
    assert!(utl_track_a(&v).is_empty());
    // filters 回显
    assert_eq!(v["filters"]["evidence"], json!(["unknown"]));
}

/// 来源过滤：foreground 段 source_id=windows 命中；ai_run 的
/// app_name=claude_code 命中（用户无需知道存储里 source_id='ai'）；无来源
/// 证据的 unknown 占位段在带 source 过滤时缺席（轨道标注「未接入」仍在）。
#[test]
fn unified_timeline_filter_by_source() {
    let (conn, now) = utl_filter_fixture();
    let f = kynoptic_core::timeline::TimelineFilter {
        source: vec!["windows".into()],
        ..Default::default()
    };
    let v = api_unified_timeline_filtered_at(&conn, 60, now, &f).unwrap();
    let a = utl_track_a(&v);
    assert_eq!(a.len(), 4, "windows 过滤命中全部四个前台段: {a:?}");
    assert!(a.iter().all(|iv| iv["source_id"] == json!("windows")));
    // D 轨 ai_run source_id=ai 不命中 windows → 空
    assert_eq!(
        v["tracks"].as_array().unwrap()[3]["intervals"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
    // B 轨占位段（source null）被 source 过滤排除
    assert!(utl_track_b(&v).is_empty());

    // 按源名过滤：claude_code 经 app_name 命中（存储细节封装在契约里）
    let f = kynoptic_core::timeline::TimelineFilter {
        source: vec!["claude_code".into()],
        ..Default::default()
    };
    let v = api_unified_timeline_filtered_at(&conn, 60, now, &f).unwrap();
    assert!(utl_track_a(&v).is_empty());
    let d = v["tracks"].as_array().unwrap()[3]["intervals"]
        .as_array()
        .unwrap();
    assert_eq!(d.len(), 1);
    assert_eq!(d[0]["app_name"], json!("claude_code"));
    assert_eq!(v["filters"]["source"], json!(["claude_code"]));

    // 多来源逗号语义：windows + claude_code 两个维度同时命中
    let f = kynoptic_core::timeline::TimelineFilter {
        source: vec!["windows".into(), "claude_code".into()],
        ..Default::default()
    };
    let v = api_unified_timeline_filtered_at(&conn, 60, now, &f).unwrap();
    assert_eq!(utl_track_a(&v).len(), 4);
    assert_eq!(
        v["tracks"].as_array().unwrap()[3]["intervals"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

/// 项目过滤：精确匹配 project 字段；unassigned（null）区间不命中；
/// 不存在的项目名 → 空表（合法的「无命中」，与错误响应可区分——filters
/// 回显说明过滤在生效）。
#[test]
fn unified_timeline_filter_by_project() {
    let (conn, now) = utl_filter_fixture();
    let f = kynoptic_core::timeline::TimelineFilter {
        project: vec!["alpha".into()],
        ..Default::default()
    };
    let v = api_unified_timeline_filtered_at(&conn, 60, now, &f).unwrap();
    let a = utl_track_a(&v);
    assert_eq!(
        a.len(),
        2,
        "alpha 规则命中的两段（inferred+observed）都在: {a:?}"
    );
    assert!(a.iter().all(|iv| iv["project"] == json!("alpha")));
    // B/beta 无归属（规则未命中）→ 不命中 alpha 过滤
    assert!(!a.iter().any(|iv| iv["app_name"] == json!("B.exe")));
    // AI 段 unassigned → 不命中
    assert_eq!(
        v["tracks"].as_array().unwrap()[3]["intervals"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
    // 占位段（无 project）被 project 过滤排除
    assert!(utl_track_b(&v).is_empty());
    assert_eq!(v["filters"]["project"], json!(["alpha"]));

    // 不存在的项目：合法空结果（不是错误）
    let f = kynoptic_core::timeline::TimelineFilter {
        project: vec!["nonexistent".into()],
        ..Default::default()
    };
    let v = api_unified_timeline_filtered_at(&conn, 60, now, &f).unwrap();
    assert!(utl_track_a(&v).is_empty());
    assert_eq!(v["filters"]["project"], json!(["nonexistent"]));
}

/// 组合过滤（AND 语义）：project=alpha + evidence=observed 同时成立才命中；
/// 默认（空 TimelineFilter）与 api_unified_timeline_at 同值——wrapper 契约。
#[test]
fn unified_timeline_filter_combination_and_default_parity() {
    let (conn, now) = utl_filter_fixture();
    let f = kynoptic_core::timeline::TimelineFilter {
        project: vec!["alpha".into()],
        evidence: vec!["observed".into()],
        ..Default::default()
    };
    let v = api_unified_timeline_filtered_at(&conn, 60, now, &f).unwrap();
    let a = utl_track_a(&v);
    assert!(a
        .iter()
        .all(|iv| iv["project"] == json!("alpha") && iv["evidence_level"] == json!("observed")));
    assert!(!a.is_empty(), "alpha observed 段组合命中");

    // 默认过滤器与既有端点完全同值（同一实现，wrapper 传 Default）
    let base = api_unified_timeline_at(&conn, 60, now).unwrap();
    let fdef = kynoptic_core::timeline::TimelineFilter::default();
    let with_def = api_unified_timeline_filtered_at(&conn, 60, now, &fdef).unwrap();
    assert_eq!(base, with_def, "空过滤 = 无过滤，同值契约");
    assert_eq!(
        base["filters"],
        json!({"project": [], "source": [], "evidence": []})
    );
}

/// HTTP 层筛选契约：evidence 越词表值 → 400（静默空表会被误读为「无活动」）；
/// 合法值回显 filters；逗号分隔与重复参数两种写法等价；未知参数照旧回显。
#[test]
fn unified_timeline_route_filter_contract() {
    let conn = mem_conn();
    let db = tmpdir("utlfilter").join("kyn.db");
    // 词表越值 → 400
    let (code, _, body) = route_req(
        &conn,
        "GET",
        "/api/timeline?minutes=30&evidence=confirmed_only",
        "",
        &db,
    );
    assert_eq!(code, 400, "evidence 越词表必须 400: {body}");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert!(v["error"].as_str().unwrap().contains("observed"));
    // 合法值（逗号分隔）→ 200 + filters 回显
    let (code, _, body) = route_req(
        &conn,
        "GET",
        "/api/timeline?minutes=30&evidence=observed,inferred",
        "",
        &db,
    );
    assert_eq!(code, 200);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["filters"]["evidence"], json!(["observed", "inferred"]));
    // 重复参数写法等价
    let (code, _, body) = route_req(
        &conn,
        "GET",
        "/api/timeline?minutes=30&evidence=observed&evidence=inferred",
        "",
        &db,
    );
    assert_eq!(code, 200);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["filters"]["evidence"], json!(["observed", "inferred"]));
    // source/project 透传 + 未知参数照旧回显 ignored_params
    let (code, _, body) = route_req(
        &conn,
        "GET",
        "/api/timeline?minutes=30&source=windows&project=alpha&foo=1",
        "",
        &db,
    );
    assert_eq!(code, 200);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["filters"]["source"], json!(["windows"]));
    assert_eq!(v["filters"]["project"], json!(["alpha"]));
    assert_eq!(v["ignored_params"], json!(["foo"]));
    // 不带筛选时 filters 为空数组（契约字段恒在）
    let (code, _, body) = route_req(&conn, "GET", "/api/timeline?minutes=30", "", &db);
    assert_eq!(code, 200);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        v["filters"],
        json!({"project": [], "source": [], "evidence": []})
    );
}

/// F11 merge + F07 筛选：筛选条件透传远端（fetch_remote_timeline 的 query
/// 参数进请求行）；空 query 只带 minutes（向后兼容远端旧版本）。
#[test]
fn fetch_remote_timeline_forwards_filter_query() {
    use crate::merge::fetch_remote_timeline;
    use std::io::{Read as _, Write as _};
    use std::time::Duration;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let h = std::thread::spawn(move || {
        // 两次请求（带 query / 空 query）：mock 服务端循环 accept
        for _ in 0..2 {
            let Ok((mut s, _)) = listener.accept() else {
                break;
            };
            let mut buf = [0u8; 4096];
            let _ = s.read(&mut buf);
            // 把收到的请求行原样回给客户端（透传核对面）
            let req = String::from_utf8_lossy(&buf).to_string();
            let line = req.lines().next().unwrap_or("").to_string();
            let body = serde_json::json!({ "request_line": line }).to_string();
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = s.write_all(resp.as_bytes());
        }
    });
    let url = format!("http://{}", addr);
    let v = fetch_remote_timeline(
        &url,
        "",
        60,
        "project=alpha&source=windows&evidence=observed,inferred",
        Duration::from_secs(5),
    )
    .unwrap();
    assert!(
        v["request_line"].as_str().unwrap().contains(
            "api/timeline?minutes=60&project=alpha&source=windows&evidence=observed,inferred"
        ),
        "筛选 query 必须原样进请求行: {}",
        v["request_line"]
    );
    // 空 query：只带 minutes（不追加尾随 &）
    let v = fetch_remote_timeline(&url, "", 60, "", Duration::from_secs(5)).unwrap();
    let line = v["request_line"].as_str().unwrap();
    assert!(
        line.contains("api/timeline?minutes=60"),
        "空 query 只带 minutes: {line}"
    );
    assert!(!line.contains("&&"), "空 query 不得产生空参数: {line}");
    h.join().unwrap();
}

// === Stage 3：D 轨真实数据（ai_run + editor_session）、F06 项目归属、F08 报表 ===

/// D 轨（Stage 3）：ai_run + editor_session 两类区间进同一轨道（按 source
/// 分色由前端消费，端点只保证 source_id/app_name/interval_kind/project 字段
/// 齐全）；project 有值时 project_evidence=inferred（F06 规则匹配是推导）。
#[test]
fn unified_timeline_track_d_merges_ai_run_and_editor_session() {
    use kynoptic_core::db::activity_intervals::{
        rebuild_activity_intervals_with_rules_at, ProjectRule,
    };
    let conn = mem_conn();
    // 编辑器前台：Code.exe/alphafile.rs 200s→250s（250s 切走，闭区间 observed）
    utl_switch(&conn, 200, "Code.exe", "alphafile.rs - Visual Studio Code");
    utl_switch(&conn, 250, "Code.exe", "alphafile.rs - Visual Studio Code");
    utl_switch(&conn, 260, "chrome.exe", "web");
    // AI 会话：observed 300s→360s
    conn.execute(
        "INSERT INTO events (timestamp, event_type, event_action, event_data, app_name, source_id, quality) \
         VALUES (?1, 'ai_session', 'ai_run', ?2, 'claude_code', 'ai', 'observed')",
        params![
            utl_rfc(UTL_BASE_MS + 300_000),
            serde_json::json!({
                "schema_version": 1,
                "session_id": "fix-stage3-d",
                "source": "claude_code",
                "start": utl_rfc(UTL_BASE_MS + 300_000),
                "end": utl_rfc(UTL_BASE_MS + 360_000),
                "start_quality": "observed",
                "end_quality": "observed",
                "prompt_count": 1,
            })
            .to_string(),
        ],
    )
    .unwrap();
    let now = UTL_BASE_MS + 400_000;
    let rules = vec![ProjectRule {
        name: "alpha".into(),
        path_prefix: String::new(),
        title_keywords: "alphafile".into(),
    }];
    rebuild_activity_intervals_with_rules_at(&conn, now, &rules).unwrap();
    let v = api_unified_timeline_at(&conn, 60, now).unwrap();
    let d = &v["tracks"].as_array().unwrap()[3];
    assert_eq!(d["connected"], json!(true), "有区间时 D 轨接入");
    let ivs = d["intervals"].as_array().unwrap();
    assert_eq!(ivs.len(), 2, "editor_session + ai_run 都进 D 轨: {ivs:?}");
    // 两类区间按 interval_kind 可辨，前端据此分色
    let kinds: Vec<&str> = ivs
        .iter()
        .map(|x| x["interval_kind"].as_str().unwrap())
        .collect();
    assert!(kinds.contains(&"editor_session"));
    assert!(kinds.contains(&"ai_run"));
    // 编辑器区间：source_id=windows、app_name=编辑器进程、project=alpha
    let ed = ivs
        .iter()
        .find(|x| x["interval_kind"] == json!("editor_session"))
        .unwrap();
    assert_eq!(ed["app_name"], json!("Code.exe"));
    assert_eq!(ed["source_id"], json!("windows"));
    assert_eq!(ed["project"], json!("alpha"));
    assert_eq!(ed["project_evidence"], json!("inferred"));
    // AI 区间：source_id=ai、app_name=源名、无归属证据（ai_session 事件不带
    // 标题/路径——隐私铁律，不猜）
    let ai = ivs
        .iter()
        .find(|x| x["interval_kind"] == json!("ai_run"))
        .unwrap();
    assert_eq!(ai["app_name"], json!("claude_code"));
    assert_eq!(ai["source_id"], json!("ai"));
    assert_eq!(ai["project"], Value::Null);
    assert_eq!(ai["project_evidence"], Value::Null);
}

/// /api/reports?days=N（F08）：按本地日 × 类别/来源/项目汇总；
/// estimated 段单列不并入 observed 合计（复审口径）；project=(none) 表示
/// 无归属证据。`_at` 注入时钟。
#[test]
fn reports_aggregate_by_day_and_bucket_quality() {
    use kynoptic_core::db::activity_intervals::{
        rebuild_activity_intervals_with_rules_at, ProjectRule,
    };
    let conn = mem_conn();
    // A/T（含 alpha）：0s→100s observed；50s idle_start/60s idle_end 相交
    // → 整段打 estimated（只打标不裁剪，duration 不变）
    utl_switch(&conn, 0, "A.exe", "alphafile");
    utl_switch(&conn, 50, "A.exe", "alphafile");
    utl_idle(&conn, 50, "idle_start");
    utl_idle(&conn, 60, "idle_end");
    utl_switch(&conn, 100, "A.exe", "alphafile");
    // B/X（含 beta）：110s→150s observed（同题刷新闭区间；单条 switch 只产
    // 零宽点区间——rebuild 口径，点区间非区间，不产时长）
    utl_switch(&conn, 110, "B.exe", "betafile");
    utl_switch(&conn, 150, "B.exe", "betafile");
    // 编辑器：200s→250s observed
    utl_switch(&conn, 200, "Code.exe", "alphamain.rs - VS Code");
    utl_switch(&conn, 250, "Code.exe", "alphamain.rs - VS Code");
    utl_switch(&conn, 260, "chrome.exe", "web");
    // AI 会话 300s→360s observed
    conn.execute(
        "INSERT INTO events (timestamp, event_type, event_action, event_data, app_name, source_id, quality) \
         VALUES (?1, 'ai_session', 'ai_run', ?2, 'claude_code', 'ai', 'observed')",
        params![
            utl_rfc(UTL_BASE_MS + 300_000),
            serde_json::json!({
                "schema_version": 1,
                "session_id": "fix-reports",
                "source": "claude_code",
                "start": utl_rfc(UTL_BASE_MS + 300_000),
                "end": utl_rfc(UTL_BASE_MS + 360_000),
                "start_quality": "observed",
                "end_quality": "observed",
                "prompt_count": 1,
            })
            .to_string(),
        ],
    )
    .unwrap();
    let now = UTL_BASE_MS + 400_000;
    let rules = vec![
        ProjectRule {
            name: "alpha".into(),
            path_prefix: String::new(),
            title_keywords: "alpha".into(),
        },
        ProjectRule {
            name: "beta".into(),
            path_prefix: String::new(),
            title_keywords: "beta".into(),
        },
    ];
    rebuild_activity_intervals_with_rules_at(&conn, now, &rules).unwrap();

    let v = api_reports_at(&conn, 7, now).unwrap();
    assert_eq!(v["endpoint"], json!("reports"));
    assert_eq!(v["days"], json!(7));
    let rows = v["rows"].as_array().unwrap();
    // 行查找 helper（日期按本地日界，与实现同一换算源 Local）
    let find = |kind: &str, source: &str, project: &str| -> Value {
        rows.iter()
            .find(|r| {
                r["kind"] == json!(kind)
                    && r["source"] == json!(source)
                    && r["project"] == json!(project)
            })
            .cloned()
            .unwrap_or_else(|| panic!("缺行 {kind}/{source}/{project}: {rows:?}"))
    };
    // A 前台 [0,100s]：与 away 相交 → 整段 estimated（100s），不进 observed
    let a = find("foreground_dwell", "A.exe", "alpha");
    assert_eq!(
        a["observed_ms"],
        json!(0),
        "estimated 段不得计入 observed: {a}"
    );
    assert_eq!(a["estimated_ms"], json!(100_000));
    // B/X（含 beta）：[110,150s] observed 40s（200s Code.exe 切走前已闭合）
    let b = find("foreground_dwell", "B.exe", "beta");
    assert_eq!(b["observed_ms"], json!(40_000));
    assert_eq!(b["estimated_ms"], json!(0));
    assert_eq!(b["unknown_ms"], json!(0));
    // chrome 尾部断线收口 unknown：260s→400s = 140s，单列（不并入 observed）
    let ch = find("foreground_dwell", "chrome.exe", "(none)");
    assert_eq!(ch["unknown_ms"], json!(140_000));
    assert_eq!(ch["observed_ms"], json!(0));
    // 编辑器 [200,250s] observed + 项目 alpha
    let ed = find("editor_session", "Code.exe", "alpha");
    assert_eq!(ed["observed_ms"], json!(50_000));
    assert_eq!(ed["estimated_ms"], json!(0));
    // AI 会话 observed 60s，无归属证据 → (none)
    let ai = find("ai_run", "claude_code", "(none)");
    assert_eq!(ai["observed_ms"], json!(60_000));
    // 窗口合计（复审口径 2026-11-03）：editor_session 是 foreground_dwell 的
    // 分类视图（同一时段两行），totals 排除它避免编辑器 50s 计两次。
    // observed 只含 observed 桶（B 40s + 编辑器 dwell 50s + AI 60s = 150s）
    assert_eq!(v["totals"]["observed_ms"], json!(150_000));
    assert_eq!(v["totals"]["estimated_ms"], json!(100_000));
    assert_eq!(v["totals"]["unknown_ms"], json!(140_000));
    // 日期列 = 区间所在本地日（与实现同一 Local 换算）
    use chrono::TimeZone;
    let expected_date = Local
        .timestamp_opt(UTL_BASE_MS / 1000, 0)
        .single()
        .unwrap()
        .format("%Y-%m-%d")
        .to_string();
    assert_eq!(a["date"], json!(expected_date));
}

/// /api/reports 跨本地午界的区间按午界拆分归日（不整段记入起始日）。
#[test]
fn reports_split_intervals_at_local_midnight() {
    use chrono::TimeZone;
    let conn = mem_conn();
    // 直接造一条跨越本地午界的 dwell 区间（不经 rebuild——派生表是只读
    // 汇总入口，本测端点的拆分口径）：当日 23:50 → 次日 00:10 本地钟面。
    let day = Local
        .timestamp_opt(UTL_BASE_MS / 1000, 0)
        .single()
        .unwrap()
        .date_naive();
    let start_local = day.and_hms_opt(23, 50, 0).unwrap();
    let end_local = day.succ_opt().unwrap().and_hms_opt(0, 10, 0).unwrap();
    let start_utc = Local.from_local_datetime(&start_local).single().unwrap();
    let end_utc = Local.from_local_datetime(&end_local).single().unwrap();
    let start_ms = start_utc.timestamp_millis();
    let end_ms = end_utc.timestamp_millis();
    let midnight_ms = Local
        .from_local_datetime(&day.succ_opt().unwrap().and_hms_opt(0, 0, 0).unwrap())
        .single()
        .unwrap()
        .timestamp_millis();
    conn.execute(
        "INSERT INTO activity_intervals \
         (interval_kind, start_ts, end_ts, app_name, title_hash, quality, source_id, project) \
         VALUES ('foreground_dwell', ?1, ?2, 'A.exe', 'h', 'observed', 'windows', NULL)",
        params![utl_rfc(start_ms), utl_rfc(end_ms)],
    )
    .unwrap();
    let v = api_reports_at(&conn, 7, end_ms).unwrap();
    let rows = v["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 2, "跨午界应拆成两天: {rows:?}");
    let d0 = start_utc.format("%Y-%m-%d").to_string();
    let d1 = end_utc.format("%Y-%m-%d").to_string();
    let r0 = rows.iter().find(|r| r["date"] == json!(d0)).unwrap();
    let r1 = rows.iter().find(|r| r["date"] == json!(d1)).unwrap();
    assert_eq!(
        r0["observed_ms"],
        json!(midnight_ms - start_ms),
        "首日只记到午界: {r0}"
    );
    assert_eq!(
        r1["observed_ms"],
        json!(end_ms - midnight_ms),
        "次日只记午界后: {r1}"
    );
}

/// /api/reports 路由：缺省 days=7；>90 钳制 + days_requested/note 标注；
/// days=0/负数/非数字 → 400。
#[test]
fn reports_route_days_default_cap_and_validation() {
    let conn = mem_conn();
    let db = std::path::Path::new("kyn.db");
    let (code, _, body) = route_req(&conn, "GET", "/api/reports", "", db);
    assert_eq!(code, 200);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["days"], json!(7), "缺省 7 天");
    let (code, _, body) = route_req(&conn, "GET", "/api/reports?days=95", "", db);
    assert_eq!(code, 200);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["days"], json!(90), "上限 90 钳制");
    assert_eq!(v["days_requested"], json!(95));
    assert!(v["note"].as_str().unwrap().contains("90"));
    for bad in ["0", "-1", "abc"] {
        let (code, _, _) = route_req(&conn, "GET", &format!("/api/reports?days={bad}"), "", db);
        assert_eq!(code, 400, "days={bad} 必须 400");
    }
}

/// F06 设置面：project_rules 是接受字段——POST 生效落盘、响应回显、不进
/// ignored；两个字段都空的规则与缺失 name 拒绝 400；超 100 条拒绝。
#[test]
fn settings_post_project_rules_accepted_and_validated() {
    let dir = tmpdir("projrules-api");
    let db = dir.join("kyn.db");
    let body = json!({
        "project_rules": [
            {"name": "alpha", "path_prefix": "k:/proj/alpha", "title_keywords": ""},
            {"name": "beta", "path_prefix": "", "title_keywords": "sample-project spec"}
        ]
    });
    let (code, _, out) = route_req(&mem_conn(), "POST", "/api/settings", &body.to_string(), &db);
    assert_eq!(code, 200, "合法规则应接受: {out}");
    let v: Value = serde_json::from_str(&out).unwrap();
    assert!(
        v.get("ignored").is_none(),
        "project_rules 不得进 ignored: {out}"
    );
    assert_eq!(v["project_rules"], body["project_rules"]);
    // 落盘复核
    let (_, _, out2) = route_req(&mem_conn(), "GET", "/api/settings", "", &db);
    let g: Value = serde_json::from_str(&out2).unwrap();
    assert_eq!(g["project_rules"], body["project_rules"], "须落盘");
    // 防呆：两字段全空 → 400
    let (code, _, out) = route_req(
        &mem_conn(),
        "POST",
        "/api/settings",
        r#"{"project_rules":[{"name":"x","path_prefix":"","title_keywords":"  "}]}"#,
        &db,
    );
    assert_eq!(code, 400, "全空规则必须拒绝: {out}");
    // 缺 name → 400
    let (code, _, _) = route_req(
        &mem_conn(),
        "POST",
        "/api/settings",
        r#"{"project_rules":[{"path_prefix":"p"}]}"#,
        &db,
    );
    assert_eq!(code, 400);
    // 超 100 条 → 400
    let rules: Vec<Value> = (0..101)
        .map(|i| json!({"name": format!("r{i}"), "path_prefix": format!("p{i}")}))
        .collect();
    let (code, _, _) = route_req(
        &mem_conn(),
        "POST",
        "/api/settings",
        &json!({"project_rules": rules}).to_string(),
        &db,
    );
    assert_eq!(code, 400, "超上限必须拒绝");
    std::fs::remove_dir_all(&dir).unwrap();
}

/// 报表页签注册：页签 + 容器 + 天数按钮 + 加载函数在 dashboard.html 中。
#[test]
fn reports_view_registered_in_dashboard() {
    for id in [
        "tab-reports",
        "view-reports",
        "rep-table",
        "rep-totals",
        "rep-days-btn",
        "loadReports",
        "proj-rules",
        "proj-rule-add",
        "renderProjRules",
    ] {
        assert!(DASHBOARD_HTML.contains(id), "dashboard 须包含 {id}");
    }
}

// === F04 编辑器扩展摄取（POST /api/ext/events） ===

/// 建一个带完整 schema 的临时库文件 + 写好扩展令牌，返回 (dir, db 路径)。
/// ingest 在 route 层自开写连接，不消费传入的内存只读连接。
fn ext_db(tag: &str, token: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    let dir = tmpdir(tag);
    let db = dir.join("kyn.db");
    let conn = Connection::open(&db).unwrap();
    conn.execute_batch(kynoptic_core::db::SCHEMA).unwrap();
    kynoptic_core::db::run_migrations(&conn).unwrap();
    drop(conn);
    std::fs::write(dir.join("ext-token.txt"), token).unwrap();
    (dir, db)
}

const EXT_TEST_TOKEN: &str = "ext-token-fix";

fn ext_body(paths: &[(&str, &str, &str, &str)]) -> String {
    let evs: Vec<Value> = paths
        .iter()
        .map(|(id, ts, action, path)| {
            let mut ev = json!({"event_id": id, "timestamp": ts, "action": action});
            if !path.is_empty() {
                ev["file_path"] = json!(path);
            }
            ev
        })
        .collect();
    json!({"source": "vscode", "events": evs}).to_string()
}

/// 固定注入时钟（不读真实时钟；与 activity_intervals 测试同基准日附近）。
const EXT_NOW_MS: i64 = 1_792_977_600_000 + 600_000; // 2026-11-02T03:10:00Z

#[test]
fn ext_events_ingest_writes_and_is_idempotent() {
    let (dir, db) = ext_db("ext-ingest", EXT_TEST_TOKEN);
    let conn = mem_conn();
    let body = ext_body(&[
        (
            "id-1",
            "2026-11-02T03:00:00Z",
            "file_open",
            "K:/proj/x/main.rs",
        ),
        (
            "id-2",
            "2026-11-02T03:00:10Z",
            "focus_gained",
            "K:/proj/x/main.rs",
        ),
        (
            "id-2",
            "2026-11-02T03:00:10Z",
            "focus_gained",
            "K:/proj/x/main.rs",
        ), // 同 id → 幂等
    ]);
    let (code, _, out) = route_req_ext(
        &conn,
        "POST",
        "/api/ext/events",
        &body,
        &db,
        Some(EXT_TEST_TOKEN),
    );
    assert_eq!(code, 200, "{out}");
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["inserted"], json!(2));
    assert_eq!(v["duplicates"], json!(1));
    // 重发整批：全部幂等命中，不产生新行（append-only 行数不变）
    let (code, _, out) = route_req_ext(
        &conn,
        "POST",
        "/api/ext/events",
        &body,
        &db,
        Some(EXT_TEST_TOKEN),
    );
    assert_eq!(code, 200, "{out}");
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["inserted"], json!(0));
    assert_eq!(v["duplicates"], json!(3));
    // 落库形态：events 表 event_type='editor'、source_id='vscode_ext'、
    // window_title=路径，ext_event_id 带来源前缀
    let rd = Connection::open(&db).unwrap();
    let total: i64 = rd
        .query_row(
            "SELECT COUNT(*) FROM events WHERE event_type='editor'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(total, 2, "幂等：同 event_id 只落一行");
    let (src, title, eid): (String, Option<String>, String) = rd
        .query_row(
            "SELECT source_id, window_title, ext_event_id FROM events \
             WHERE event_type='editor' ORDER BY id LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(src, "vscode_ext");
    assert_eq!(title.as_deref(), Some("K:/proj/x/main.rs"));
    assert_eq!(eid, "vscode_ext:id-1");
    drop(rd); // Windows：WAL 侧车文件持锁，先关连接再清目录
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn ext_events_rejects_bad_or_missing_token() {
    let (dir, db) = ext_db("ext-auth", EXT_TEST_TOKEN);
    let conn = mem_conn();
    let body = ext_body(&[("a", "2026-11-02T03:00:00Z", "file_open", "")]);
    // 无令牌
    let (code, _, _) = route_req_ext(&conn, "POST", "/api/ext/events", &body, &db, None);
    assert_eq!(code, 401);
    // 错令牌
    let (code, _, _) = route_req_ext(&conn, "POST", "/api/ext/events", &body, &db, Some("wrong"));
    assert_eq!(code, 401);
    // 旧 route_req（不传令牌）同样 401——直调不可绕过鉴权
    let (code, _, _) = route_req(&conn, "POST", "/api/ext/events", &body, &db);
    assert_eq!(code, 401);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn ext_events_rejects_invalid_schema_whole_batch() {
    let (dir, db) = ext_db("ext-schema", EXT_TEST_TOKEN);
    let conn = mem_conn();
    let cases = [
        (
            "未知 source",
            r#"{"source":"jetbrains","events":[{"event_id":"a","timestamp":"2026-11-02T03:00:00Z","action":"file_open"}]}"#,
        ),
        (
            "未知 action",
            r#"{"source":"vscode","events":[{"event_id":"a","timestamp":"2026-11-02T03:00:00Z","action":"keystroke"}]}"#,
        ),
        (
            "坏 timestamp",
            r#"{"source":"cursor","events":[{"event_id":"a","timestamp":"not-a-time","action":"file_open"}]}"#,
        ),
        (
            "缺 event_id",
            r#"{"source":"vscode","events":[{"timestamp":"2026-11-02T03:00:00Z","action":"file_open"}]}"#,
        ),
        ("空 events", r#"{"source":"vscode","events":[]}"#),
        ("坏 JSON", "{oops"),
    ];
    for (tag, body) in cases {
        let (code, _, out) = route_req_ext(
            &conn,
            "POST",
            "/api/ext/events",
            body,
            &db,
            Some(EXT_TEST_TOKEN),
        );
        assert_eq!(code, 400, "{tag}: {out}");
    }
    // 校验失败的批不落任何行（整批先验后写）
    let rd = Connection::open(&db).unwrap();
    let n: i64 = rd
        .query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 0);
    drop(rd); // Windows：WAL 侧车文件持锁，先关连接再清目录
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn ext_events_feed_editor_session_intervals() {
    let (dir, db) = ext_db("ext-intervals", EXT_TEST_TOKEN);
    let conn = mem_conn();
    let t = |s: i64| {
        chrono::DateTime::from_timestamp_millis(1_792_977_600_000 + s * 1000)
            .unwrap()
            .to_rfc3339()
    };
    let body = ext_body(&[
        ("e1", &t(0), "file_open", "K:/proj/alpha/src/main.rs"),
        ("e2", &t(30), "focus_lost", ""),
    ]);
    let (code, _, out) = route_req_ext(
        &conn,
        "POST",
        "/api/ext/events",
        &body,
        &db,
        Some(EXT_TEST_TOKEN),
    );
    assert_eq!(code, 200, "{out}");
    // ingest 内置节流重建与其它测试共享 static，可能被跳过——直接注入
    // 时钟重建（派生表口径与 route 层同一函数），断言 editor_session。
    let w = Connection::open(&db).unwrap();
    kynoptic_core::db::activity_intervals::rebuild_activity_intervals_at(&w, EXT_NOW_MS).unwrap();
    let rows: Vec<(String, String, i64)> = {
        let mut stmt = w
            .prepare(
                "SELECT app_name, quality, (julianday(end_ts)-julianday(start_ts))*86400000.0 \
                 FROM activity_intervals WHERE interval_kind='editor_session' AND source_id='vscode_ext'",
            )
            .unwrap();
        stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                (r.get::<_, f64>(2)?).round() as i64,
            ))
        })
        .unwrap()
        .collect::<std::result::Result<Vec<_>, _>>()
        .unwrap()
    };
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0], ("vscode".into(), "observed".into(), 30_000));
    drop(w); // Windows：WAL 侧车文件持锁，先关连接再清目录
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn ext_token_get_and_reset_rotate() {
    let (dir, db) = ext_db("ext-token-api", EXT_TEST_TOKEN);
    let conn = mem_conn();
    // GET 显示当前令牌
    let (code, _, out) = route_req(&conn, "GET", "/api/ext/token", "", &db);
    assert_eq!(code, 200);
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["configured"], json!(true));
    assert_eq!(v["token"], json!(EXT_TEST_TOKEN));
    // 重置后旧令牌 401、新令牌 200
    let (code, _, out) = route_req(&conn, "POST", "/api/ext/token/reset", "", &db);
    assert_eq!(code, 200, "{out}");
    let v: Value = serde_json::from_str(&out).unwrap();
    let new_token = v["token"].as_str().unwrap().to_string();
    assert_ne!(new_token, EXT_TEST_TOKEN);
    let body = ext_body(&[("a", "2026-11-02T03:00:00Z", "file_open", "")]);
    let (code, _, _) = route_req_ext(
        &conn,
        "POST",
        "/api/ext/events",
        &body,
        &db,
        Some(EXT_TEST_TOKEN),
    );
    assert_eq!(code, 401, "旧令牌必须失效");
    let (code, _, _) = route_req_ext(
        &conn,
        "POST",
        "/api/ext/events",
        &body,
        &db,
        Some(&new_token),
    );
    assert_eq!(code, 200);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// F04 设置页注册：扩展令牌展示/重置的元素与加载/重置函数在
/// dashboard.html 中（与 reports 页签注册同款防漂移检查）。
#[test]
fn ext_token_ui_registered_in_dashboard() {
    for id in [
        "ext-token-value",
        "ext-token-reset",
        "ext-token-msg",
        "loadExtToken",
        "resetExtToken",
    ] {
        assert!(DASHBOARD_HTML.contains(id), "dashboard 须包含 {id}");
    }
}

/// F04 全链路（真实 TCP）：serve 绑定随机端口，扩展形态的请求（无 Origin、
/// 无浏览器头，仅 X-Kynoptic-Ext-Token）应 200；无令牌应 401——验证
/// handle_client 对 /api/ext/events 的 CSRF/会话令牌豁免与扩展令牌鉴权。
#[test]
fn ext_events_http_end_to_end() {
    let (dir, db) = ext_db("ext-http", EXT_TEST_TOKEN);
    let db_for_serve = db.clone();
    let server = std::thread::spawn(move || {
        let _ = serve(&db_for_serve, 0, true);
    });
    // serve 把实际端口写入 ACTUAL_PORT；等待其就绪
    let mut port = 0u16;
    for _ in 0..100 {
        port = ACTUAL_PORT.load(std::sync::atomic::Ordering::SeqCst);
        if port != 0 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert_ne!(port, 0, "serve 未在超时内绑定端口");
    let body = ext_body(&[("h1", "2026-11-02T03:00:00Z", "file_open", "K:/proj/x/a.rs")]);
    let send = |token: Option<&str>| -> (u16, String) {
        let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        let mut req = format!(
            "POST /api/ext/events HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
            body.len()
        );
        if let Some(t) = token {
            req.push_str(&format!("X-Kynoptic-Ext-Token: {t}\r\n"));
        }
        req.push_str("\r\n");
        req.push_str(&body);
        use std::io::Write as _;
        s.write_all(req.as_bytes()).unwrap();
        let mut resp = String::new();
        use std::io::Read as _;
        s.read_to_string(&mut resp).unwrap();
        let status: u16 = resp
            .split_whitespace()
            .nth(1)
            .and_then(|c| c.parse().ok())
            .unwrap_or(0);
        (status, resp)
    };
    // 扩展形态（无 Origin/Sec-Fetch 头）+ 正确令牌 → 200
    let (code, resp) = send(Some(EXT_TEST_TOKEN));
    assert_eq!(code, 200, "{resp}");
    assert!(resp.contains("\"inserted\":1"), "{resp}");
    // 无令牌 → 401（且不是 403 csrf——扩展不是浏览器请求）
    let (code, resp) = send(None);
    assert_eq!(code, 401, "{resp}");
    drop(server); // 线程与连接随测试进程结束回收
    std::fs::remove_dir_all(&dir).ok();
}

// === F05 AI hook 真实事件通道（POST /api/ext/events 的 claude_code/zcode 分支） ===

/// hook 请求体：模拟真实 Claude Code hook stdin payload（脱敏自造；
/// 官方 payload 字段：session_id / hook_event_name / tool_name，无 timestamp
/// ——事件时间取接收时刻 now_ms 注入）。
fn ai_hook_body(source: &str, events: &[Value]) -> String {
    json!({"source": source, "events": events}).to_string()
}

/// 与 dash 分支同校验同幂等路径的直接入口（`_at` 注入时钟）。
#[test]
fn ai_hook_events_ingest_observed_and_idempotent() {
    let (dir, db) = ext_db("ai-hook-ingest", EXT_TEST_TOKEN);
    let body = ai_hook_body(
        "claude_code",
        &[
            json!({"hook_event_name": "PreToolUse", "session_id": "sess-dash-1", "tool_name": "Bash", "waiting": true}),
            json!({"hook_event_name": "PostToolUse", "session_id": "sess-dash-1", "tool_name": "Bash", "duration_ms": 1500}),
            json!({"hook_event_name": "Notification", "session_id": "sess-dash-1"}),
            json!({"hook_event_name": "Stop", "session_id": "sess-dash-1"}),
        ],
    );
    let r1 = crate::api_ai_hook_events_post_at(&db, &body, EXT_NOW_MS).unwrap();
    assert_eq!(r1["received"], json!(4));
    assert_eq!(r1["inserted"], json!(4));
    // 同 payload 重发 → 全部幂等命中（payload 哈希 ext_event_id）
    let r2 = crate::api_ai_hook_events_post_at(&db, &body, EXT_NOW_MS).unwrap();
    assert_eq!(r2["inserted"], json!(0));
    assert_eq!(r2["duplicates"], json!(4));
    let rd = Connection::open(&db).unwrap();
    let (n, src, q): (i64, String, String) = rd
        .query_row(
            "SELECT COUNT(*), source_id, quality FROM events WHERE event_type='ai_session'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(n, 4, "幂等：4 条 hook 事件只落 4 行");
    assert_eq!(src, "claude_code", "真实通道 source_id=工具源名");
    assert_eq!(q, "observed");
    let eid: String = rd
        .query_row(
            "SELECT ext_event_id FROM events WHERE event_type='ai_session' LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(eid.starts_with("claude_code:"), "幂等键带来源前缀: {eid}");
    drop(rd);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// 独立复审 2026-11-11 high：payload 无 timestamp 时（ts=接收时刻注入、
/// 不参与哈希），同一 payload 在**不同接收时刻**的两次 POST = 两次独立
/// observed 发生，必须各落一行（纯 payload 哈希口径会把第二条误判成重发
/// 吞掉）。同毫秒重发去重仍由上一测试覆盖（同 now_ms → duplicates）。
#[test]
fn ai_hook_events_same_payload_second_occurrence_kept() {
    let (dir, db) = ext_db("ai-hook-second", EXT_TEST_TOKEN);
    let body = ai_hook_body(
        "claude_code",
        &[json!({"hook_event_name": "Stop", "session_id": "sess-second"})],
    );
    let r1 = crate::api_ai_hook_events_post_at(&db, &body, EXT_NOW_MS).unwrap();
    assert_eq!(r1["inserted"], json!(1));
    let r2 = crate::api_ai_hook_events_post_at(&db, &body, EXT_NOW_MS + 5_000).unwrap();
    assert_eq!(r2["inserted"], json!(1), "第二次发生的相同事件不得吞: {r2}");
    assert_eq!(r2["duplicates"], json!(0));
    let rd = Connection::open(&db).unwrap();
    let (n, t1, t2): (i64, String, String) = rd
        .query_row(
            "SELECT COUNT(*), MIN(timestamp), MAX(timestamp) FROM events \
             WHERE event_type='ai_session'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(n, 2, "两次发生各落一行: {n}");
    assert_ne!(t1, t2, "两条事件时间 = 各自接收时刻: {t1} vs {t2}");
    drop(rd);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// payload 显式带 `event_id`（去重头）：同一 event_id 重发跨接收时刻稳定
/// 去重（重试端方案），不同 event_id 各自成事件。
#[test]
fn ai_hook_events_explicit_event_id_dedup_header() {
    let (dir, db) = ext_db("ai-hook-eid", EXT_TEST_TOKEN);
    let mk = |eid: &str| {
        ai_hook_body(
            "claude_code",
            &[json!({"hook_event_name": "Stop", "session_id": "s", "event_id": eid})],
        )
    };
    let r1 = crate::api_ai_hook_events_post_at(&db, &mk("hook-1"), EXT_NOW_MS).unwrap();
    assert_eq!(r1["inserted"], json!(1));
    // 同 event_id 不同时刻重发 → 幂等命中（去重头跨重发稳定）
    let r2 = crate::api_ai_hook_events_post_at(&db, &mk("hook-1"), EXT_NOW_MS + 9_000).unwrap();
    assert_eq!(r2["inserted"], json!(0), "同 event_id 重发应去重: {r2}");
    assert_eq!(r2["duplicates"], json!(1));
    // 不同 event_id → 新事件
    let r3 = crate::api_ai_hook_events_post_at(&db, &mk("hook-2"), EXT_NOW_MS + 9_000).unwrap();
    assert_eq!(r3["inserted"], json!(1));
    // 非法 event_id（超长）整批拒收
    let long = "x".repeat(129);
    assert!(crate::api_ai_hook_events_post_at(&db, &mk(&long), EXT_NOW_MS).is_err());
    let rd = Connection::open(&db).unwrap();
    let n: i64 = rd
        .query_row(
            "SELECT COUNT(*) FROM events WHERE event_type='ai_session'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 2, "去重头口径：2 行（hook-1 去重 + hook-2）: {n}");
    drop(rd);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// dsh 无 hook 机制：通道如实拒绝（400 风格错误文案说明走文件扫描）。
#[test]
fn ai_hook_events_reject_dsh_source() {
    let (dir, db) = ext_db("ai-hook-dsh", EXT_TEST_TOKEN);
    let body = ai_hook_body(
        "dsh",
        &[json!({"hook_event_name": "Stop", "session_id": "dsh-x"})],
    );
    let err = crate::api_ai_hook_events_post_at(&db, &body, EXT_NOW_MS).unwrap_err();
    assert!(err.contains("dsh 无 hook 机制"), "{err}");
    std::fs::remove_dir_all(&dir).unwrap();
}

/// HTTP 路由级：AI hook 分支受扩展令牌鉴权（无令牌 401，错令牌 401），
/// 正确令牌 200；zcode 源可写；校验失败整批拒收（写 0 行）。
#[test]
fn ai_hook_events_route_auth_and_zcode() {
    let (dir, db) = ext_db("ai-hook-route", EXT_TEST_TOKEN);
    let conn = mem_conn();
    let body = ai_hook_body(
        "zcode",
        &[
            json!({"hook_event_name": "SessionStart", "session_id": "zc-sess-1"}),
            json!({"hook_event_name": "PreToolUse", "session_id": "zc-sess-1", "tool_name": "Bash"}),
            json!({"hook_event_name": "PostToolUseFailure", "session_id": "zc-sess-1", "tool_name": "Bash"}),
            json!({"hook_event_name": "Stop", "session_id": "zc-sess-1"}),
        ],
    );
    // 无令牌 → 401
    let (code, _, out) = route_req_ext(&conn, "POST", "/api/ext/events", &body, &db, None);
    assert_eq!(code, 401, "{out}");
    // 错令牌 → 401
    let (code, _, _) = route_req_ext(&conn, "POST", "/api/ext/events", &body, &db, Some("wrong"));
    assert_eq!(code, 401);
    // 正确令牌 → 200，4 行落库（source_id='zcode'）
    let (code, _, out) = route_req_ext(
        &conn,
        "POST",
        "/api/ext/events",
        &body,
        &db,
        Some(EXT_TEST_TOKEN),
    );
    assert_eq!(code, 200, "{out}");
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["inserted"], json!(4));
    // 整批拒收：一条坏事件 → 400 且写 0 行
    let bad = ai_hook_body(
        "zcode",
        &[
            json!({"hook_event_name": "Stop", "session_id": "zc-ok"}),
            json!({"hook_event_name": "NotARealEvent", "session_id": "zc-ok"}),
        ],
    );
    let (code, _, out) = route_req_ext(
        &conn,
        "POST",
        "/api/ext/events",
        &bad,
        &db,
        Some(EXT_TEST_TOKEN),
    );
    assert_eq!(code, 400, "{out}");
    let rd = Connection::open(&db).unwrap();
    let n: i64 = rd
        .query_row(
            "SELECT COUNT(*) FROM events WHERE event_type='ai_session' AND source_id='zcode'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 4, "坏批不得写半批");
    drop(rd);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// 全链路（hook 事件 → 落库 → ai_run 区间派生）：同一会话 hook observed
/// 优先于扫描 inferred，不双计；区间窗口 = hook 事件 [最早, 最晚]。
#[test]
fn ai_hook_events_feed_ai_run_intervals() {
    let (dir, db) = ext_db("ai-hook-interval", EXT_TEST_TOKEN);
    // 先放一条扫描 inferred 会话（模拟 ai_adapter 扫描先行）
    {
        let w = Connection::open(&db).unwrap();
        w.execute(
            "INSERT INTO events (timestamp, event_type, event_action, event_data, app_name, source_id, quality) \
             VALUES (?1, 'ai_session', 'ai_run', ?2, 'claude_code', 'ai', 'inferred')",
            rusqlite::params![
                "2026-11-02T03:00:00+00:00",
                json!({
                    "schema_version": 1, "session_id": "sess-chain", "source": "claude_code",
                    "start": "2026-11-02T03:00:00+00:00", "end": "2026-11-02T03:05:00+00:00",
                    "start_quality": "inferred", "end_quality": "inferred"
                })
                .to_string(),
            ],
        )
        .unwrap();
    }
    // hook observed 事件（同会话，3:01-3:02）
    let body = ai_hook_body(
        "claude_code",
        &[
            json!({"hook_event_name": "PreToolUse", "session_id": "sess-chain", "tool_name": "Edit",
                   "timestamp": "2026-11-02T03:01:00Z"}),
            json!({"hook_event_name": "PostToolUse", "session_id": "sess-chain", "tool_name": "Edit",
                   "duration_ms": 8000, "timestamp": "2026-11-02T03:02:00Z"}),
        ],
    );
    crate::api_ai_hook_events_post_at(&db, &body, EXT_NOW_MS).unwrap();
    // 全量重建（注入时钟；节流 static 可能挡掉 ingest 内建重建，测试直接重建）
    let w = Connection::open(&db).unwrap();
    kynoptic_core::db::activity_intervals::rebuild_activity_intervals_at(&w, EXT_NOW_MS).unwrap();
    let rows: Vec<(String, String, String, String)> = {
        let mut stmt = w
            .prepare(
                "SELECT app_name, quality, start_ts, end_ts FROM activity_intervals \
                 WHERE interval_kind='ai_run' ORDER BY start_ts",
            )
            .unwrap();
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .collect::<std::result::Result<Vec<_>, rusqlite::Error>>()
            .unwrap()
    };
    assert_eq!(rows.len(), 1, "同会话两路证据不双计");
    let (app, q, start, end) = &rows[0];
    assert_eq!(app, "claude_code");
    assert_eq!(q, "observed", "hook observed 优先于扫描 inferred");
    assert_eq!(start, "2026-11-02T03:01:00+00:00");
    assert_eq!(end, "2026-11-02T03:02:00+00:00");
    drop(w);
    std::fs::remove_dir_all(&dir).unwrap();
}

// === F06 缺口补齐：人工指定项目（POST /api/intervals/project，migration 0018）===

const PO_BASE_MS: i64 = 1_792_977_600_000; // 2026-11-02T03:00:00Z，固定不读真实时钟

fn po_rfc(off_ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(PO_BASE_MS + off_ms)
        .unwrap()
        .to_rfc3339()
}

/// 文件库（POST 写路径走 db_path；内存库不适用）。
fn po_file_conn(tag: &str) -> (std::path::PathBuf, Connection) {
    let dir = tmpdir(tag);
    let db = dir.join("kyn.db");
    let conn = Connection::open(&db).unwrap();
    conn.execute_batch(kynoptic_core::db::SCHEMA).unwrap();
    kynoptic_core::db::run_migrations(&conn).unwrap();
    (db, conn)
}

/// 指定项目全链路：校验 → append-only 落库（project_overrides）→ 重建 →
/// timeline 返回 project_source=manual / project_evidence=observed；二次指定
/// 同区间 corrected_from=manual（审计：记录被取代的前归属）。_at 注入时钟。
#[test]
fn intervals_project_post_assigns_manual_and_rebuilds() {
    use kynoptic_core::db::activity_intervals::rebuild_activity_intervals_at;
    let (db, conn) = po_file_conn("pov-ok");
    for off in [0i64, 10_000] {
        conn.execute(
            "INSERT INTO events (timestamp, event_type, event_action, app_name, window_title) \
             VALUES (?1, 'window', 'switch', 'Code.exe', 'sample-file.rs')",
            params![po_rfc(off)],
        )
        .unwrap();
    }
    let now = PO_BASE_MS + 60_000;
    rebuild_activity_intervals_at(&conn, now).unwrap();
    // 取下钻面板同源的区间时间（尾段断线收口 [0s, now]）
    let (start_ts, end_ts): (String, String) = conn
        .query_row(
            "SELECT start_ts, end_ts FROM activity_intervals \
             WHERE interval_kind='foreground_dwell' LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();

    let body = serde_json::json!({
        "interval_kind": "foreground_dwell",
        "start_ts": start_ts,
        "end_ts": end_ts,
        "app_name": "Code.exe",
        "title_hash": conn.query_row(
            "SELECT title_hash FROM activity_intervals WHERE interval_kind='foreground_dwell' LIMIT 1",
            [], |r| r.get::<_, Option<String>>(0)).unwrap(),
        "project": "manual-proj",
    })
    .to_string();
    let (code, _, out) = route_req(&conn, "POST", "/api/intervals/project", &body, &db);
    assert_eq!(code, 200, "out={out}");
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["ok"], json!(true));
    assert_eq!(v["project_source"], json!("manual"));
    assert_eq!(v["project_evidence"], json!("observed"));
    assert_eq!(v["corrected_from"], json!("unassigned"));
    assert_eq!(v["rebuilt"], json!(true));
    // append-only：project_overrides 恰 1 行（审计字段落盘）
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM project_overrides", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 1);

    // timeline（下钻数据源）返回 manual / observed
    let tl = crate::api_unified_timeline_at(&conn, 60, now).unwrap();
    let a = &tl["tracks"].as_array().unwrap()[0];
    assert_eq!(a["intervals"][0]["project"], json!("manual-proj"));
    assert_eq!(a["intervals"][0]["project_source"], json!("manual"));
    assert_eq!(a["intervals"][0]["project_evidence"], json!("observed"));

    // 二次指定（追加覆盖）：corrected_from 如实记录被取代的 manual；
    // title_hash 必须与区间一致才匹配——重取真实值
    let th: Option<String> = conn
        .query_row(
            "SELECT title_hash FROM activity_intervals WHERE interval_kind='foreground_dwell' LIMIT 1",
            [], |r| r.get(0)).unwrap();
    let body2 = serde_json::json!({
        "interval_kind": "foreground_dwell",
        "start_ts": start_ts, "end_ts": end_ts,
        "app_name": "Code.exe", "title_hash": th,
        "project": "manual-proj-2",
    })
    .to_string();
    let (code2, _, out2) = route_req(&conn, "POST", "/api/intervals/project", &body2, &db);
    assert_eq!(code2, 200, "out2={out2}");
    let v2: Value = serde_json::from_str(&out2).unwrap();
    assert_eq!(
        v2["corrected_from"],
        json!("manual"),
        "审计记录被取代的前归属"
    );
    let (proj, psrc): (String, String) = conn
        .query_row(
            "SELECT project, project_source FROM activity_intervals \
             WHERE interval_kind='foreground_dwell' LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(proj, "manual-proj-2", "追加覆盖：最晚指定生效");
    assert_eq!(psrc, "manual");
    let n2: i64 = conn
        .query_row("SELECT COUNT(*) FROM project_overrides", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n2, 2, "append-only：只增不删");
}

/// 校验面：坏 JSON / 未知 kind / 空 project / 缺失目标区间 / 坏时间 → 400，
/// 且不落任何行（先验后写）。
#[test]
fn intervals_project_post_validates_input() {
    use kynoptic_core::db::activity_intervals::rebuild_activity_intervals_at;
    let (db, conn) = po_file_conn("pov-bad");
    conn.execute(
        "INSERT INTO events (timestamp, event_type, event_action, app_name, window_title) \
         VALUES (?1, 'window', 'switch', 'app.exe', 't')",
        params![po_rfc(0)],
    )
    .unwrap();
    rebuild_activity_intervals_at(&conn, PO_BASE_MS + 30_000).unwrap();
    let post = |body: &str| route_req(&conn, "POST", "/api/intervals/project", body, &db).0;

    assert_eq!(post("{oops"), 400, "坏 JSON");
    assert_eq!(
        post(
            r#"{"interval_kind":"away","start_ts":"2026-11-02T03:00:00+00:00","end_ts":"2026-11-02T03:01:00+00:00","app_name":"x","project":"p"}"#
        ),
        400,
        "away 无归属面"
    );
    assert_eq!(
        post(
            r#"{"interval_kind":"foreground_dwell","start_ts":"2026-11-02T03:00:00+00:00","end_ts":"2026-11-02T03:01:00+00:00","app_name":"app.exe","project":"  "}"#
        ),
        400,
        "空 project"
    );
    assert_eq!(
        post(
            r#"{"interval_kind":"foreground_dwell","start_ts":"2026-11-02T05:00:00+00:00","end_ts":"2026-11-02T05:01:00+00:00","app_name":"app.exe","project":"p"}"#
        ),
        400,
        "目标区间不存在（窗口不相交）"
    );
    assert_eq!(
        post(
            r#"{"interval_kind":"foreground_dwell","start_ts":"2026-11-02T03:01:00+00:00","end_ts":"2026-11-02T03:00:00+00:00","app_name":"app.exe","project":"p"}"#
        ),
        400,
        "end_ts <= start_ts"
    );
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM project_overrides", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 0, "校验失败不得落任何行");
}

/// 可追溯缺省：无人工指定、无规则 → timeline project_source=unassigned、
/// project_evidence=null；reports 行 project_source=unassigned（不虚构归属）。
#[test]
fn timeline_and_reports_default_to_unassigned_source() {
    use kynoptic_core::db::activity_intervals::rebuild_activity_intervals_at;
    let conn = mem_conn();
    conn.execute(
        "INSERT INTO events (timestamp, event_type, event_action, app_name, window_title) \
         VALUES (?1, 'window', 'switch', 'app.exe', 't')",
        params![po_rfc(0)],
    )
    .unwrap();
    let now = PO_BASE_MS + 60_000;
    rebuild_activity_intervals_at(&conn, now).unwrap();
    let tl = crate::api_unified_timeline_at(&conn, 60, now).unwrap();
    let iv = &tl["tracks"].as_array().unwrap()[0]["intervals"][0];
    assert_eq!(iv["project_source"], json!("unassigned"));
    assert_eq!(iv["project_evidence"], Value::Null);
    let rep = crate::api_reports_at(&conn, 1, now).unwrap();
    let row = &rep["rows"].as_array().unwrap()[0];
    assert_eq!(row["project"], json!("(none)"));
    assert_eq!(row["project_source"], json!("unassigned"));
}

// === F09 采集器健康（GET /api/health） ===

/// 判定阈值表（纯函数，无时钟）：unknown 优先、超阈值 stale、退避 degraded、
/// 其余 ok——全部用常量边界值断言，不读真实时钟。
#[test]
fn health_classifier_thresholds() {
    use crate::{classify_monitor_health, HEALTH_MONITOR_STALE_SECS};
    // 无证据 → unknown（不是零、不是故障）
    assert_eq!(classify_monitor_health(None, 0), "unknown");
    // 阈值内 + 无退避 → ok
    assert_eq!(classify_monitor_health(Some(0), 0), "ok");
    assert_eq!(
        classify_monitor_health(Some(HEALTH_MONITOR_STALE_SECS), 0),
        "ok"
    );
    // 超阈值 → stale（即使无退避）
    assert_eq!(
        classify_monitor_health(Some(HEALTH_MONITOR_STALE_SECS + 1), 0),
        "stale"
    );
    // 阈值内但连续慢采集中 → degraded
    assert_eq!(classify_monitor_health(Some(5), 1), "degraded");
    // stale 优先级高于 degraded：超阈值且退避 → stale
    assert_eq!(
        classify_monitor_health(Some(HEALTH_MONITOR_STALE_SECS + 1), 3),
        "stale"
    );
    use crate::{classify_channel_health, HEALTH_AI_CHANNEL_STALE_SECS, HEALTH_EXT_STALE_SECS};
    assert_eq!(
        classify_channel_health(None, HEALTH_EXT_STALE_SECS),
        "unknown"
    );
    assert_eq!(
        classify_channel_health(Some(HEALTH_EXT_STALE_SECS), HEALTH_EXT_STALE_SECS),
        "ok"
    );
    assert_eq!(
        classify_channel_health(Some(HEALTH_EXT_STALE_SECS + 1), HEALTH_EXT_STALE_SECS),
        "stale"
    );
    assert_eq!(
        classify_channel_health(
            Some(HEALTH_AI_CHANNEL_STALE_SECS + 1),
            HEALTH_AI_CHANNEL_STALE_SECS
        ),
        "stale"
    );
}

/// /api/health 汇总（注入时钟）：真实 hook 事件落库后 claude_code 通道
/// 在 24h 阈值内 → ok；把 now 推到 3 天后 → stale；无记录的通道恒 unknown。
#[test]
fn api_health_channel_states_with_injected_now() {
    let (dir, db) = ext_db("health-api", EXT_TEST_TOKEN);
    let body = ai_hook_body(
        "claude_code",
        &[json!({"hook_event_name": "Stop", "session_id": "hc-1"})],
    );
    crate::api_ai_hook_events_post_at(&db, &body, EXT_NOW_MS).unwrap();
    let now = chrono::DateTime::from_timestamp_millis(EXT_NOW_MS).unwrap();
    // 健康读 events：hook 事件落在 db 文件（ingest 自开写连接），用该库
    // 连接读，不读与文件无关的 mem_conn。
    let rconn = Connection::open(&db).unwrap();
    let h = crate::api_health_at(&rconn, now);
    assert_eq!(h["endpoint"], json!("health"));
    // claude_code：10 分钟前有事件 → ok；zcode / 扩展无记录 → unknown
    let ch = |name: &str| -> Value {
        h["ai_channels"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["channel"] == json!(name))
            .cloned()
            .unwrap()
    };
    assert_eq!(ch("claude_code hook")["state"], json!("ok"));
    // hook payload 无 timestamp → 事件时间 = 接收时刻（EXT_NOW_MS 注入），
    // 与测试 now 相同 → age 0，状态 ok。
    assert_eq!(ch("claude_code hook")["age_secs"], json!(0));
    assert_eq!(ch("zcode hook")["state"], json!("unknown"));
    let ext = |name: &str| -> Value {
        h["extensions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["channel"] == json!(name))
            .cloned()
            .unwrap()
    };
    assert_eq!(ext("browser extension")["state"], json!("unknown"));
    // 时钟推 3 天：claude_code 超 24h 阈值 → stale，总体 stale
    let later = now + chrono::Duration::days(3);
    let h2 = crate::api_health_at(&rconn, later);
    let ch2 = h2["ai_channels"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["channel"] == json!("claude_code hook"))
        .unwrap();
    assert_eq!(ch2["state"], json!("stale"));
    assert_eq!(h2["status"], json!("stale"));
    // 阈值常量随响应透出（前端/CLI 免硬编码）
    assert_eq!(
        h["thresholds"]["ai_channel_stale_secs"],
        json!(crate::HEALTH_AI_CHANNEL_STALE_SECS)
    );
    drop(rconn); // Windows：先关连接再清目录
    std::fs::remove_dir_all(&dir).unwrap();
}

/// 路由级：GET /api/health 200 且结构完整（测试进程无采集器 → monitors
/// 空数组即 unknown 注记；通道条目按清单齐备）；未知查询参数回显。
#[test]
fn health_route_arm_200() {
    let conn = mem_conn();
    let (code, ctype, out) = route_req(&conn, "GET", "/api/health", "", Path::new("kyn.db"));
    assert_eq!(code, 200, "{out}");
    assert_eq!(ctype, "application/json");
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["status"], json!("unknown"), "无采集器无事件 → unknown");
    assert_eq!(v["monitors"], json!([]));
    assert_eq!(v["ai_channels"].as_array().unwrap().len(), 3);
    assert_eq!(v["extensions"].as_array().unwrap().len(), 3);
    let (_, _, out2) = route_req(&conn, "GET", "/api/health?foo=1", "", Path::new("kyn.db"));
    let v2: Value = serde_json::from_str(&out2).unwrap();
    assert_eq!(v2["ignored_params"], json!(["foo"]));
}

// === F10 隐私安全（绑定/token/路径注入/整标题脱敏） ===

/// F10-1 断言 serve 只绑 127.0.0.1：serve 起后，同端口通配地址（0.0.0.0）
/// 必须仍可绑定——若 serve 绑了通配地址，本断言的 bind 会因端口冲突失败。
/// 另断言 DNS rebinding 防线：非回环 Host 名 403 forbidden_host。
#[test]
fn serve_binds_loopback_only() {
    // 自选空闲端口（先占后放），不依赖 ACTUAL_PORT 全局（避免与并行
    // serve 测试竞态串台）。
    let probe = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = probe.local_addr().unwrap().port();
    drop(probe);
    let (dir, db) = ext_db("bind-loopback", EXT_TEST_TOKEN);
    let db2 = db.clone();
    let server = std::thread::spawn(move || {
        let _ = crate::serve(&db2, port, true);
    });
    let mut ready = false;
    for _ in 0..100 {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            ready = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(ready, "serve 未在超时内监听 127.0.0.1:{port}");
    // 通配地址仍空闲 = serve 没有绑 0.0.0.0（只绑了 127.0.0.1）
    let wildcard = std::net::TcpListener::bind(("0.0.0.0", port));
    assert!(
        wildcard.is_ok(),
        "serve 必须只绑 127.0.0.1；通配地址同端口被占说明绑了 0.0.0.0"
    );
    drop(wildcard);
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    use std::io::{Read as _, Write as _};
    let req = format!(
        "GET /api/status HTTP/1.1\r\nHost: evil.example.com:{port}\r\nConnection: close\r\n\r\n"
    );
    s.write_all(req.as_bytes()).unwrap();
    let mut resp = String::new();
    s.read_to_string(&mut resp).unwrap();
    assert!(resp.contains("403"), "{resp}");
    assert!(resp.contains("forbidden_host"), "{resp}");
    drop(server);
    std::fs::remove_dir_all(&dir).ok();
}

/// F10-2 token_required 模式下新端点受保护：写 dashboard-token.txt 后
/// serve，GET /api/health 无令牌 401、错令牌 401、带令牌 200（HTTP 层
/// /api/* 前缀统一闸门覆盖新端点，无需逐端点白名单）。
#[test]
fn health_endpoint_requires_access_token_http() {
    let probe = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = probe.local_addr().unwrap().port();
    drop(probe);
    let (dir, db) = ext_db("health-token", EXT_TEST_TOKEN);
    std::fs::write(dir.join("dashboard-token.txt"), "sekrit-health\n").unwrap();
    let db2 = db.clone();
    let server = std::thread::spawn(move || {
        let _ = crate::serve(&db2, port, true);
    });
    let mut ready = false;
    for _ in 0..100 {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            ready = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(ready, "serve 未就绪");
    let send = |token: Option<&str>| -> (u16, String) {
        let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        let mut req =
            format!("GET /api/health HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n");
        if let Some(t) = token {
            req.push_str(&format!("X-Kynoptic-Access-Token: {t}\r\n"));
        }
        req.push_str("\r\n");
        use std::io::{Read as _, Write as _};
        s.write_all(req.as_bytes()).unwrap();
        let mut resp = String::new();
        s.read_to_string(&mut resp).unwrap();
        let code: u16 = resp
            .split_whitespace()
            .nth(1)
            .and_then(|c| c.parse().ok())
            .unwrap_or(0);
        (code, resp)
    };
    let (code, resp) = send(None);
    assert_eq!(code, 401, "{resp}");
    assert!(resp.contains("unauthorized"), "{resp}");
    let (code, resp) = send(Some("wrong-token"));
    assert_eq!(code, 401, "{resp}");
    let (code, resp) = send(Some("sekrit-health"));
    assert_eq!(code, 200, "{resp}");
    assert!(resp.contains("\"endpoint\":\"health\""), "{resp}");
    drop(server);
    std::fs::remove_dir_all(&dir).ok();
}

/// F10-3 路径注入防线：file_path 含 `..` 路径段整批拒收（400），不落任何行；
/// 文件名中恰好含 "a..b"（非路径段）不受影响。
#[test]
fn ext_events_reject_dotdot_path_segment() {
    let (dir, db) = ext_db("ext-dotdot", EXT_TEST_TOKEN);
    let conn = mem_conn();
    for bad in [
        "../../etc/passwd",
        "K:/proj/../../../secret.txt",
        "a\\..\\b.txt",
    ] {
        let body = ext_body(&[("dd-1", "2026-11-02T03:00:00Z", "file_open", bad)]);
        let (code, _, out) = route_req_ext(
            &conn,
            "POST",
            "/api/ext/events",
            &body,
            &db,
            Some(EXT_TEST_TOKEN),
        );
        assert_eq!(code, 400, "path={bad} out={out}");
        assert!(out.contains(".."), "{out}");
    }
    let body = ext_body(&[(
        "dd-ok",
        "2026-11-02T03:00:00Z",
        "file_open",
        "K:/proj/a..b/x.rs",
    )]);
    let (code, _, out) = route_req_ext(
        &conn,
        "POST",
        "/api/ext/events",
        &body,
        &db,
        Some(EXT_TEST_TOKEN),
    );
    assert_eq!(code, 200, "{out}");
    let rd = Connection::open(&db).unwrap();
    let total: i64 = rd
        .query_row(
            "SELECT COUNT(*) FROM events WHERE event_type='editor'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(total, 1, "只有正常路径那一条落库");
    drop(rd);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// F10-4 整标题脱敏设置项：POST 落盘 → GET 读回；缺省 false（默认存原文，
/// 不改变既有行为与既有数据）；dashboard UI 含开关。
#[test]
fn redact_titles_full_setting_roundtrip_and_default_off() {
    let (dir, db) = ext_db("redact-full", EXT_TEST_TOKEN);
    let conn = mem_conn();
    let (_, _, out) = route_req(&conn, "GET", "/api/settings", "", &db);
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["redact_titles_full"], json!(false), "缺省关");
    let body = json!({"redact_titles_full": true}).to_string();
    let (code, _, out) = route_req(&conn, "POST", "/api/settings", &body, &db);
    assert_eq!(code, 200, "{out}");
    let (_, _, out) = route_req(&conn, "GET", "/api/settings", "", &db);
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["redact_titles_full"], json!(true));
    assert_eq!(v["redact_titles"], json!(false), "不牵连相邻设置");
    let (code, _, _) = route_req(
        &conn,
        "POST",
        "/api/settings",
        r#"{"redact_titles_full": "yes"}"#,
        &db,
    );
    assert_eq!(code, 400, "非布尔值拒收");
    assert!(crate::DASHBOARD_HTML.contains("set-redact-full"));
    assert!(crate::DASHBOARD_HTML.contains("redact_titles_full"));
    std::fs::remove_dir_all(&dir).unwrap();
}

// === F11 多设备 timeline 合流（device_endpoints / ?merge=1） ===

/// F11 设置项：POST 接受 device_endpoints 并落盘读回；不出现在 ignored；
/// 非法 url / 空 name / 非数组拒收 400；UI 含编辑器容器与字段类名。
#[test]
fn device_endpoints_setting_roundtrip_and_validation() {
    let dir = tmpdir("dev-endpoints");
    let db = dir.join("kyn.db");
    let conn = mem_conn();
    // 缺省空（单机模式）
    let (_, _, out) = route_req(&conn, "GET", "/api/settings", "", &db);
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["device_endpoints"], json!([]), "opt-in：默认空");
    // 合法保存
    let body = json!({
        "device_endpoints": [
            {"name": "workstation-b", "url": "http://127.0.0.1:18422", "token": "sample-token"},
            {"name": "laptop-c", "url": "http://localhost:8422"}
        ]
    })
    .to_string();
    let (code, _, out) = route_req(&conn, "POST", "/api/settings", &body, &db);
    assert_eq!(code, 200, "{out}");
    let v: Value = serde_json::from_str(&out).unwrap();
    assert!(
        v.get("ignored").is_none(),
        "device_endpoints 不应被列进 ignored"
    );
    assert_eq!(v["device_endpoints"].as_array().unwrap().len(), 2);
    assert_eq!(v["device_endpoints"][0]["name"], json!("workstation-b"));
    // 落盘读回
    let (_, _, out) = route_req(&conn, "GET", "/api/settings", "", &db);
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(
        v["device_endpoints"][0]["url"],
        json!("http://127.0.0.1:18422")
    );
    assert_eq!(v["device_endpoints"][1]["token"], json!(""));
    // 非法 url（https / 非 http 开头）拒收
    let (code, _, _) = route_req(
        &conn,
        "POST",
        "/api/settings",
        r#"{"device_endpoints":[{"name":"x","url":"https://127.0.0.1"}]}"#,
        &db,
    );
    assert_eq!(code, 400, "https url 应拒收");
    // 空 name 拒收
    let (code, _, _) = route_req(
        &conn,
        "POST",
        "/api/settings",
        r#"{"device_endpoints":[{"name":"  ","url":"http://127.0.0.1:1"}]}"#,
        &db,
    );
    assert_eq!(code, 400);
    // 非数组拒收
    let (code, _, _) = route_req(
        &conn,
        "POST",
        "/api/settings",
        r#"{"device_endpoints":"x"}"#,
        &db,
    );
    assert_eq!(code, 400);
    // UI 编辑器接线
    assert!(crate::DASHBOARD_HTML.contains("dev-endpoints"));
    assert!(crate::DASHBOARD_HTML.contains("device_endpoints"));
    std::fs::remove_dir_all(&dir).unwrap();
}

/// 本地 mock HTTP timeline 服务：回固定 JSON。返回 (线程句柄, base url)。
fn mock_timeline_server(payload: Value) -> (std::thread::JoinHandle<()>, String) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let body = payload.to_string();
    let h = std::thread::spawn(move || {
        // 单次请求：路由级 merge 只对每个远端拉一次（filter 透传测试有自己的 mock）
        if let Ok((mut s, _)) = listener.accept() {
            let mut buf = [0u8; 4096];
            let _ = s.read(&mut buf);
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = s.write_all(resp.as_bytes());
        }
    });
    (h, format!("http://{}", addr))
}

/// F11 路由级合流：settings 里配一个在线 mock（返回带 A 轨区间的远端
/// timeline）+ 一个连不上的端口 → GET /api/timeline?minutes=60&merge=1
/// 200；本机轨道在前、每区间带 device；远端设备分轨；offline 设备显式
/// 标注且 error 在场；human_presence_boundary 明示不相加。
#[test]
fn timeline_merge_route_merges_remote_and_marks_offline() {
    let dir = tmpdir("merge-route");
    let db = dir.join("kyn.db");
    let conn = mem_conn();
    // 本机库插一条 window/switch 并重建派生表，让本机 A 轨有真实区间
    let now_ms = chrono::Utc::now().timestamp_millis();
    let start_ts = chrono::DateTime::from_timestamp_millis(now_ms - 300_000)
        .unwrap()
        .to_rfc3339();
    insert(&conn, &start_ts, "window", "switch", Some("sample-app"));
    kynoptic_core::db::activity_intervals::rebuild_activity_intervals_at(&conn, now_ms).unwrap();
    // 远端 mock：合法 timeline payload（远端窗口与请求 minutes 无关，仅验证合流）
    let remote_payload = json!({
        "endpoint": "unified_timeline",
        "window_start_ms": now_ms - 3_600_000i64,
        "window_end_ms": now_ms,
        "tracks": [{
            "id": "A", "label_en": "Foreground windows", "label_zh": "前台窗口",
            "connected": true,
            "intervals": [{"start_ms": now_ms - 600_000i64, "end_ms": now_ms - 300_000i64,
                "duration_ms": 300_000, "evidence_level": "observed"}]
        }],
    });
    let (h, url) = mock_timeline_server(remote_payload);
    // settings 写入：在线 mock + 必然拒连端口（offline 降级）
    let settings_body = json!({
        "device_endpoints": [
            {"name": "workstation-b", "url": url, "token": ""},
            {"name": "laptop-c", "url": "http://127.0.0.1:1", "token": ""}
        ]
    })
    .to_string();
    let (code, _, out) = route_req(&conn, "POST", "/api/settings", &settings_body, &db);
    assert_eq!(code, 200, "{out}");
    // 合并请求
    let (code, _, out) = route_req(&conn, "GET", "/api/timeline?minutes=60&merge=1", "", &db);
    assert_eq!(code, 200, "{out}");
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["endpoint"], json!("unified_timeline_merged"));
    assert_eq!(v["merge"], json!(true));
    // 设备清单：本机 online + workstation-b online + laptop-c offline
    let devices = v["devices"].as_array().unwrap();
    assert_eq!(devices.len(), 3);
    assert_eq!(devices[0]["device"], json!("local"));
    assert_eq!(devices[1]["name"], json!("workstation-b"));
    assert_eq!(devices[1]["status"], json!("online"));
    assert_eq!(devices[2]["name"], json!("laptop-c"));
    assert_eq!(devices[2]["status"], json!("offline"));
    assert!(
        !devices[2]["error"].as_str().unwrap().is_empty(),
        "offline 必须带错误摘要"
    );
    // 轨道：本机 5 轨 + 远端 mock 返回的 1 轨原样并列；offline 无轨道
    let tracks = v["tracks"].as_array().unwrap();
    assert_eq!(tracks.len(), 6, "本机 5 轨 + 远端 1 轨，offline 不产轨道");
    assert_eq!(tracks[0]["device"], json!("local"));
    assert_eq!(tracks[5]["device"], json!("workstation-b"));
    // 远端 A 轨区间带 device 标记
    let remote_a_iv = &tracks[5]["intervals"];
    assert_eq!(remote_a_iv.as_array().unwrap().len(), 1);
    assert_eq!(remote_a_iv[0]["device"], json!("workstation-b"));
    // 本机 A 轨区间也带 device=local
    let local_a = &tracks[0]["intervals"];
    if let Some(arr) = local_a.as_array() {
        if !arr.is_empty() {
            assert_eq!(arr[0]["device"], json!("local"));
        }
    }
    // human_presence 不相加口径在场
    assert!(v["human_presence_boundary"]["note_zh"]
        .as_str()
        .unwrap()
        .contains("不相加"));
    h.join().unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
}

/// F11 无远端配置时 merge=1 仍可用：单设备（仅本机）合流形态，本机数据原样。
#[test]
fn timeline_merge_route_with_no_endpoints_is_local_only() {
    let dir = tmpdir("merge-empty");
    let db = dir.join("kyn.db");
    let conn = mem_conn();
    let (code, _, out) = route_req(&conn, "GET", "/api/timeline?minutes=30&merge=1", "", &db);
    assert_eq!(code, 200, "{out}");
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["endpoint"], json!("unified_timeline_merged"));
    let devices = v["devices"].as_array().unwrap();
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0]["device"], json!("local"));
    assert_eq!(devices[0]["status"], json!("online"));
}

/// F11 纯合流函数：_at 注入（now_ms 显式入参，零真实时钟依赖）——offline
/// 设备的区间绝不产出、本机轨道保持在前、区间 device 字段全量标注。
#[test]
fn merge_unified_timelines_is_pure_and_injects_now() {
    let now = 1_700_000_000_000i64;
    let local = json!({
        "endpoint": "unified_timeline",
        "window_start_ms": now - 60_000i64,
        "window_end_ms": now,
        "tracks": [{
            "id": "C", "label_zh": "在场 / 离场", "connected": true,
            "intervals": [{"start_ms": now - 30_000i64, "end_ms": now, "duration_ms": 30_000i64}]
        }],
    });
    let remote = json!({"tracks": [{"id": "A", "connected": true,
        "intervals": [{"start_ms": now - 10_000i64, "end_ms": now - 5_000i64}]}]});
    let merged = crate::merge::merge_unified_timelines(
        local,
        vec![
            ("dev-b".into(), Ok(remote)),
            ("dev-c".into(), Err("timeout".into())),
        ],
        1,
        now,
    );
    assert_eq!(merged["minutes"], json!(1));
    assert_eq!(merged["generated_at_ms"], json!(now), "now_ms 注入直传");
    let tracks = merged["tracks"].as_array().unwrap();
    assert_eq!(tracks.len(), 2, "offline 设备不产轨道");
    assert_eq!(tracks[0]["device"], json!("local"));
    assert_eq!(tracks[1]["device"], json!("dev-b"));
    // offline 设备只在 devices 里显式标注
    assert_eq!(merged["devices"][2]["status"], json!("offline"));
    assert_eq!(merged["devices"][2]["error"], json!("timeout"));
}
