//! 行为分析模块的单元测试
//!
//! 纯业务函数（focus_segments_from_stats / fragmentation_from_minutes /
//! apm_series_from_stats）直接喂纯数据测试，不依赖 DB。
//! 编排层 analyze_day 仍用 in-memory DB 端到端验证。

use kynoptic_core::analyzer::{
    analyze_day, apm_series_from_stats, focus_segments_from_stats, fragmentation_from_minutes,
};
use kynoptic_core::queries::MinuteStat;
use rusqlite::Connection;

fn setup_test_db() -> Connection {
    let conn = Connection::open_in_memory().expect("open in-memory");
    conn.execute_batch(
        "CREATE TABLE events (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            timestamp TEXT NOT NULL,
            event_type TEXT NOT NULL,
            event_action TEXT NOT NULL,
            event_data TEXT,
            app_name TEXT,
            window_title TEXT,
            session_id INTEGER
        );
        CREATE TABLE daily_agg (
            date TEXT PRIMARY KEY,
            keys INTEGER DEFAULT 0,
            clicks INTEGER DEFAULT 0,
            active_minutes INTEGER DEFAULT 0,
            apm_avg REAL DEFAULT 0
        );",
    )
    .expect("create schema");
    conn
}

/// 工具：插入一条带时间戳的事件
fn insert_event(conn: &Connection, ts: &str, et: &str, ea: &str) {
    conn.execute(
        "INSERT INTO events (timestamp, event_type, event_action) VALUES (?1, ?2, ?3)",
        rusqlite::params![ts, et, ea],
    )
    .expect("insert");
}

/// 构造一条 MinuteStat（switches 默认 0）
fn ms(minute: &str, keys: i64, clicks: i64) -> MinuteStat {
    MinuteStat {
        minute: minute.to_string(),
        keys,
        clicks,
        switches: 0,
    }
}

// ─── 纯函数：fragmentation_from_minutes ───────────────────────────────────────

#[test]
fn fragmentation_empty_is_zero() {
    let r = fragmentation_from_minutes(&[], "2026-06-15");
    assert_eq!(r.active_minutes, 0);
    assert_eq!(r.fragmentation_index, 0.0);
    assert_eq!(r.total_breaks, 0);
}

#[test]
fn fragmentation_single_minute_is_zero() {
    let mins = vec!["2026-06-15T10:00".to_string()];
    let r = fragmentation_from_minutes(&mins, "2026-06-15");
    assert_eq!(r.active_minutes, 1);
    assert_eq!(r.longest_streak_min, 1);
    assert_eq!(r.fragmentation_index, 0.0); // 1 - 1/1 = 0
    assert_eq!(r.total_breaks, 0);
}

#[test]
fn fragmentation_fully_continuous_is_zero() {
    // 10 连续分钟：10:00..10:09
    let mins: Vec<String> = (0..10).map(|m| format!("2026-06-15T10:{:02}", m)).collect();
    let r = fragmentation_from_minutes(&mins, "2026-06-15");
    assert_eq!(r.active_minutes, 10);
    assert_eq!(r.longest_streak_min, 10);
    assert_eq!(r.fragmentation_index, 0.0);
}

#[test]
fn fragmentation_fully_isolated_is_high() {
    // 0,5,10,15,20,25 → 6 个孤立分钟
    let mins: Vec<String> = (0..30)
        .step_by(5)
        .map(|m| format!("2026-06-15T10:{:02}", m))
        .collect();
    let r = fragmentation_from_minutes(&mins, "2026-06-15");
    assert_eq!(r.active_minutes, 6);
    assert_eq!(r.longest_streak_min, 1);
    // 1 - 1/6 ≈ 0.833
    assert!(r.fragmentation_index > 0.8);
}

// ─── 纯函数：focus_segments_from_stats ────────────────────────────────────────

#[test]
fn focus_segments_requires_min_minutes() {
    // 3 连续分钟 — 不够 5 分钟阈值
    let stats = vec![
        ms("2026-06-15T10:00", 1, 0),
        ms("2026-06-15T10:01", 1, 0),
        ms("2026-06-15T10:02", 1, 0),
    ];
    let segs = focus_segments_from_stats(&stats, |_, _| None);
    assert_eq!(segs.len(), 0);
}

#[test]
fn focus_segments_detects_5min_block() {
    // 5 连续分钟 + 一段 2 分钟隔离
    let mut stats: Vec<MinuteStat> = (0..5)
        .map(|m| ms(&format!("2026-06-15T10:{:02}", m), 1, 0))
        .collect();
    stats.push(ms("2026-06-15T11:00", 1, 0));
    stats.push(ms("2026-06-15T11:01", 1, 0));
    let segs = focus_segments_from_stats(&stats, |_, _| None);
    assert_eq!(segs.len(), 1, "expected 1 focus segment");
    assert_eq!(segs[0].duration_min, 5);
    assert_eq!(segs[0].key_count, 5);
}

#[test]
fn focus_segments_fills_app_window_via_callback() {
    let stats: Vec<MinuteStat> = (0..5)
        .map(|m| ms(&format!("2026-06-15T10:{:02}", m), 1, 0))
        .collect();
    let segs = focus_segments_from_stats(&stats, |start, _end| {
        assert_eq!(start, "2026-06-15T10:00");
        Some(("Code.exe".to_string(), "main.rs".to_string()))
    });
    assert_eq!(segs.len(), 1);
    assert_eq!(segs[0].app_name.as_deref(), Some("Code.exe"));
    assert_eq!(segs[0].window_title.as_deref(), Some("main.rs"));
}

// ─── 纯函数：apm_series_from_stats ────────────────────────────────────────────

#[test]
fn apm_series_filters_inactive_minutes() {
    let stats = vec![
        ms("2026-06-15T10:00", 1, 0),
        ms("2026-06-15T10:01", 0, 0), // 空闲，应被过滤
        ms("2026-06-15T10:02", 2, 1),
    ];
    let series = apm_series_from_stats(&stats);
    assert_eq!(series.len(), 2);
    assert_eq!(series[0].keys, 1);
    assert_eq!(series[0].clicks, 0);
    assert_eq!(series[0].apm, 1.0);
    assert_eq!(series[1].keys, 2);
    assert_eq!(series[1].clicks, 1);
    assert_eq!(series[1].apm, 3.0);
}

#[test]
fn apm_series_empty_when_all_idle() {
    let stats = vec![ms("2026-06-15T10:00", 0, 0)];
    let series = apm_series_from_stats(&stats);
    assert!(series.is_empty());
}

// ─── 编排层：analyze_day（in-memory DB 端到端） ───────────────────────────────

#[test]
fn day_analysis_aggregates_correctly() {
    let conn = setup_test_db();
    for m in 0..5 {
        insert_event(
            &conn,
            &format!("2026-06-15T10:{:02}:00Z", m),
            "keyboard",
            "press",
        );
        insert_event(
            &conn,
            &format!("2026-06-15T10:{:02}:00Z", m),
            "mouse",
            "click",
        );
    }
    let a = analyze_day(&conn, "2026-06-15").unwrap();
    assert_eq!(a.date, "2026-06-15");
    assert_eq!(a.total_keys, 5);
    assert_eq!(a.total_clicks, 5);
    assert_eq!(a.active_minutes, 5);
    assert!((a.apm_avg - 2.0).abs() < 0.1);
    assert_eq!(a.focus_segments.len(), 1);
}

// ─── daily_agg（in-memory DB，未在本轮纯化） ──────────────────────────────────

#[test]
fn daily_agg_recompute_inserts_and_updates() {
    let conn = setup_test_db();
    for m in 0..3 {
        insert_event(
            &conn,
            &format!("2026-06-15T10:{:02}:00Z", m),
            "keyboard",
            "press",
        );
    }
    let changed = kynoptic_core::daily_agg::recompute_day(&conn, "2026-06-15").unwrap();
    assert!(changed);
    let (keys, clicks, am, apm): (i64, i64, i64, f64) = conn
        .query_row(
            "SELECT keys, clicks, active_minutes, apm_avg FROM daily_agg WHERE date='2026-06-15'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert_eq!(keys, 3);
    assert_eq!(clicks, 0);
    assert_eq!(am, 3);
    assert!((apm - 1.0).abs() < 0.1);

    // 再加 1 条，重算
    insert_event(&conn, "2026-06-15T11:00:00Z", "keyboard", "press");
    let changed2 = kynoptic_core::daily_agg::recompute_day(&conn, "2026-06-15").unwrap();
    assert!(changed2);
    let keys2: i64 = conn
        .query_row(
            "SELECT keys FROM daily_agg WHERE date='2026-06-15'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(keys2, 4);
}

// ─── recompute_recent_days（基于 Local::now，维护线程用） ─────────────────────

#[test]
fn daily_agg_recompute_recent_days_covers_today_and_yesterday() {
    let conn = setup_test_db();
    // 插"本地今天"与"本地昨天"各一条按键事件。
    // 用 Local::now() 换算成 UTC RFC3339 存储，与采集器一致口径。
    let now_local = chrono::Local::now();
    let today_utc = now_local.with_timezone(&chrono::Utc).to_rfc3339();
    let yesterday_utc = (now_local - chrono::Duration::days(1))
        .with_timezone(&chrono::Utc)
        .to_rfc3339();
    insert_event(&conn, &today_utc, "keyboard", "press");
    insert_event(&conn, &yesterday_utc, "keyboard", "press");

    let n = kynoptic_core::daily_agg::recompute_recent_days(&conn, 2).unwrap();
    // 今天 + 昨天都有事件 → 2 行 daily_agg
    let rows: i64 = conn
        .query_row("SELECT COUNT(*) FROM daily_agg", [], |r| r.get(0))
        .unwrap();
    assert_eq!(rows, 2);
    assert_eq!(n, 2);

    // 幂等：再跑一次应无新增行
    let n2 = kynoptic_core::daily_agg::recompute_recent_days(&conn, 2).unwrap();
    let rows2: i64 = conn
        .query_row("SELECT COUNT(*) FROM daily_agg", [], |r| r.get(0))
        .unwrap();
    assert_eq!(rows2, 2);
    // recompute_day 在数据未变时仍返回 changed=true（INSERT ... ON CONFLICT 总改写），
    // 故 n2 仍为 2；此测试只锁定"行数稳定 + 不 panic"。
    let _ = n2;
}

#[test]
fn daily_agg_recompute_recent_days_empty_is_zero() {
    let conn = setup_test_db();
    // 无任何事件 → 仍写出 N 行（每天 0 计数），但返回 changed 计数亦为 N
    let n = kynoptic_core::daily_agg::recompute_recent_days(&conn, 3).unwrap();
    assert_eq!(n, 3);
    let rows: i64 = conn
        .query_row("SELECT COUNT(*) FROM daily_agg", [], |r| r.get(0))
        .unwrap();
    assert_eq!(rows, 3);
}
