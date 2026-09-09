//! 聚合读缓存（agg_minute / agg_daily）集成测试
//!
//! 覆盖：懒回填幂等、增量维护与全量重建等价、**原始 events 不可动**
//! （数量 + 内容指纹不变）、缓存路径与 events 现算路径结果同解、
//! input_agg 计数行对下游计数查询的兼容。

use chrono::Utc;
use kynoptic_core::db::agg;
use kynoptic_core::db::Database;
use kynoptic_core::queries;
use kynoptic_core::types::{Event, EventAction, EventType};
use serde_json::json;
use std::ops::Deref;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static SEQ: AtomicU64 = AtomicU64::new(0);

fn tmp_db_path() -> PathBuf {
    let seq = SEQ.fetch_add(1, Ordering::SeqCst);
    let mut p = std::env::temp_dir();
    p.push(format!("dp_aggtest_{}_{}.db", std::process::id(), seq));
    p
}

fn cleanup(path: &PathBuf) {
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(format!("{}-wal", path.display()));
    let _ = std::fs::remove_file(format!("{}-shm", path.display()));
}

/// events 表快照（行数 + 全行内容指纹），用于"原始数据神圣"断言。
fn events_snapshot(conn: &rusqlite::Connection) -> (i64, String) {
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))
        .unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT id||'|'||timestamp||'|'||event_type||'|'||event_action||'|'\
             ||COALESCE(event_data,'')||'|'||COALESCE(app_name,'')\
             FROM events ORDER BY id",
        )
        .unwrap();
    let hash: String = stmt
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .filter_map(|r| r.ok())
        .collect::<Vec<_>>()
        .join(";");
    (n, hash)
}

fn raw(ts: &str, t: &str, a: &str, app: Option<&str>) -> Event {
    let mut e = Event::new(
        match a {
            "press" => EventAction::Press,
            "click" => EventAction::Click,
            "switch" => EventAction::Switch,
            _ => EventAction::Heartbeat,
        },
        match t {
            "keyboard" => EventType::Keyboard,
            "mouse" => EventType::Mouse,
            "window" => EventType::Window,
            _ => EventType::System,
        },
    );
    e.timestamp = ts.into();
    if let Some(app) = app {
        e.app_name = Some(app.into());
        e.window_title = Some("t".into());
    }
    e
}

fn minute_agg_row(ts: &str, t: &str, data: serde_json::Value) -> Event {
    let mut e = Event::new(
        EventAction::InputAgg,
        if t == "keyboard" {
            EventType::Keyboard
        } else {
            EventType::Mouse
        },
    )
    .data(data);
    e.timestamp = ts.into();
    e
}

#[test]
fn counts_include_input_agg_rows() {
    let path = tmp_db_path();
    let db = Database::open(path.to_str().unwrap()).unwrap();
    let now = Utc::now().to_rfc3339();
    let today = queries::today_local_str();
    let tomorrow = queries::date_offset_str(1);
    let (start, end) = queries::local_day_range(&today).unwrap();

    let events = vec![
        raw(&now, "keyboard", "press", None), // 1 raw key
        minute_agg_row(&now, "keyboard", json!({"keys": 5, "samples": 5})), // +5 agg keys
        raw(&now, "mouse", "click", None),    // 1 raw click
        minute_agg_row(
            &now,
            "mouse",
            json!({"clicks": 3, "moves": 7, "move_distance_px": 42, "scroll_ticks": 2, "samples": 12}),
        ),
    ];
    db.insert_events(&events);

    // raw 形态行照常计数；input_agg 行按 JSON 计数求和
    assert_eq!(
        queries::count_keys_in_range(db.reader().deref(), &start, &end),
        6
    );
    assert_eq!(
        queries::count_clicks_in_range(db.reader().deref(), &start, &end),
        4
    );
    let (k, c) = queries::keys_clicks_today(db.reader().deref(), &today, &tomorrow);
    assert_eq!((k, c), (6, 4));

    // 增量聚合缓存维护（writer 线程在真实路径上做的同一件事）
    db.update_agg(&events);
    // 异常读取走 agg 缓存路径，数值与 events 现算一致
    let conn = db.reader();
    assert!(agg::has_minute_for_date(&conn, &today));
    assert_eq!(queries::late_night_key_count(&conn, &today, 0), 6); // hour>=0 = 全天
    assert_eq!(queries::day_totals(&conn, &today).keys, 6);
    drop(conn);
    cleanup(&path);
}

/// 核心契约：聚合缓存维护（增量 + 全量重建）绝不改动 events。
#[test]
fn agg_maintenance_never_touches_raw_events() {
    let path = tmp_db_path();
    let db = Database::open(path.to_str().unwrap()).unwrap();
    let events = vec![
        raw("2026-06-15T02:30:00+00:00", "keyboard", "press", None),
        raw("2026-06-15T02:30:30+00:00", "mouse", "click", None),
        raw(
            "2026-06-15T02:31:00+00:00",
            "window",
            "switch",
            Some("code.exe"),
        ),
        minute_agg_row(
            "2026-06-15T03:00:00+00:00",
            "keyboard",
            json!({"keys": 9, "samples": 9}),
        ),
    ];
    db.insert_events(&events);

    let before = events_snapshot(db.reader().deref());
    db.update_agg(&events);
    db.rebuild_agg();
    db.rebuild_agg(); // 幂等
    let after = events_snapshot(db.reader().deref());
    assert_eq!(before, after, "events 行数量与内容必须完全不变");
    cleanup(&path);
}

/// 缓存路径与 events 现算路径同解（同一份原始数据）。
/// minute 字符串格式两路径不同（UTC 截断 vs 本地桶），故对比计数数值而非字符串。
#[test]
fn cached_results_match_legacy_computation() {
    let path = tmp_db_path();
    let db = Database::open(path.to_str().unwrap()).unwrap();
    let today = queries::today_local_str();

    // 近 200 分钟逐分钟 1 次按键（马拉松形态）+ 当前分钟 300 键突增 + app 事件
    let now = Utc::now();
    let mut events = Vec::new();
    for i in 0..200 {
        let ts = (now - chrono::Duration::minutes((200 - i) as i64)).to_rfc3339();
        events.push(raw(&ts, "keyboard", "press", None));
    }
    events.push(minute_agg_row(
        &now.to_rfc3339(),
        "keyboard",
        json!({"keys": 300, "samples": 300}),
    ));
    events.push(raw(
        &now.to_rfc3339(),
        "window",
        "switch",
        Some("newapp.exe"),
    ));
    db.insert_events(&events);

    // APM 基线（history），使 apm_burst 具备触发条件
    db.with_writer(
        |conn| {
            conn.execute(
                "INSERT INTO daily_agg (date, keys, clicks, active_minutes, apm_avg)
                 VALUES (?1, 1, 1, 1, 1.0)",
                // 基线必须是"历史"日期，daily_agg_avg_apm_before 只读 date < today
                rusqlite::params![queries::date_offset_str(-1)],
            )
            .unwrap();
        },
        || panic!("写连接不可用"),
    );

    // —— 无缓存（events 现算）基准值 ——
    let legacy_day = queries::day_totals(db.reader().deref(), &today);
    let legacy_late_night = queries::late_night_key_count(db.reader().deref(), &today, 0);
    let legacy_burst_max = queries::top_burst_minutes(db.reader().deref(), &today, 100)
        .iter()
        .map(|(_, n)| *n)
        .max()
        .unwrap_or(0);

    // —— 建缓存后（agg 读路径）——
    db.update_agg(&events);
    let conn = db.reader();
    let cached_day = queries::day_totals(&conn, &today);
    let cached_late_night = queries::late_night_key_count(&conn, &today, 0);
    let cached_burst_max = queries::top_burst_minutes(&conn, &today, 100)
        .iter()
        .map(|(_, n)| *n)
        .max()
        .unwrap_or(0);

    assert_eq!(legacy_day.keys, cached_day.keys);
    assert_eq!(legacy_day.active_minutes, cached_day.active_minutes);
    assert_eq!(legacy_late_night, cached_late_night);
    assert_eq!(legacy_burst_max, cached_burst_max);
    assert_eq!(cached_burst_max, 300, "突增分钟必须被两种路径同时看到");

    // 突增检测端到端：基线 apm=1，300/1 = 300x ≥ 3x → apm_burst
    let anomalies = kynoptic_core::anomaly::detect_all(&conn, &today).unwrap();
    assert!(
        anomalies.iter().any(|a| a.kind == "apm_burst"),
        "缓存路径上 apm_burst 应触发: {anomalies:?}"
    );
    drop(conn);
    cleanup(&path);
}

/// 懒回填：agg 全空 + events 非空 → Database::open 自动补齐（模拟存量库首开）。
#[test]
fn backfill_on_open_populates_missing_cache() {
    let path = tmp_db_path();
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(kynoptic_core::db::SCHEMA).unwrap();
        kynoptic_core::db::run_migrations(&conn);
        conn.execute(
            "INSERT INTO events (timestamp, event_type, event_action, event_data, app_name, window_title, session_id)
             VALUES ('2026-06-15T12:00:00+00:00', 'keyboard', 'press', NULL, NULL, NULL, 1)",
            [],
        )
        .unwrap();
    }
    let db = Database::open(path.to_str().unwrap()).unwrap();
    let conn = db.reader();
    assert!(
        agg::has_minute_for_date(&conn, "2026-06-15"),
        "首次 open 应懒回填聚合缓存"
    );
    // 回填同样不改动 events
    assert_eq!(events_snapshot(&conn).0, 1);
    drop(conn);
    cleanup(&path);
}

/// per-app 日聚合缓存：app_history_totals / top_apps 走缓存与现算同解。
#[test]
fn app_daily_cache_matches_legacy_scan() {
    let path = tmp_db_path();
    let db = Database::open(path.to_str().unwrap()).unwrap();
    let mut events = Vec::new();
    for d in ["2026-06-01", "2026-06-02", "2026-06-03"] {
        events.push(raw(
            &format!("{d}T02:30:00+00:00"),
            "window",
            "switch",
            Some("code.exe"),
        ));
        events.push(raw(
            &format!("{d}T03:30:00+00:00"),
            "window",
            "switch",
            Some("code.exe"),
        ));
    }
    db.insert_events(&events);

    let legacy = queries::app_history_totals(db.reader().deref(), "code.exe", "2026-06-10");
    db.update_agg(&events);
    let cached = queries::app_history_totals(db.reader().deref(), "code.exe", "2026-06-10");
    assert_eq!(
        legacy, cached,
        "per-app 历史（总数, 天数）缓存与现算必须一致"
    );
    assert_eq!(cached, (6, 3));

    let top = queries::top_apps_by_event_types(db.reader().deref(), "2026-06-01", 10);
    assert_eq!(top, vec![("code.exe".to_string(), 2)]);
    cleanup(&path);
}
