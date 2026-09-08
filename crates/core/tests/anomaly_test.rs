//! 异常检测模块的单元测试
//!
//! 纯业务函数（late_night_from_count / apm_burst_from_data /
//! marathon_from_minutes / new_app_surge_from_data）直接喂纯数据测试，不依赖 DB。
//! 编排层 detect_all 用 in-memory DB 端到端验证。

use kynoptic_core::anomaly::{
    apm_burst_from_data, late_night_from_count, marathon_from_minutes, new_app_surge_from_data,
};

// ─── 纯函数：late_night_from_count ────────────────────────────────────────────

#[test]
fn late_night_below_threshold_no_alert() {
    // 阈值 50，只给 10 → 不报警
    let out = late_night_from_count(10, "2026-06-15");
    assert!(out.is_empty());
}

#[test]
fn late_night_above_threshold_alerts() {
    let out = late_night_from_count(60, "2026-06-15");
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].kind, "late_night");
    assert_eq!(out[0].severity, "warn");
    assert!(out[0].message.contains("60"));
}

#[test]
fn late_night_zero_no_alert() {
    let out = late_night_from_count(0, "2026-06-15");
    assert!(out.is_empty());
}

// ─── 纯函数：apm_burst_from_data ──────────────────────────────────────────────

#[test]
fn apm_burst_no_baseline_no_alert() {
    // hist_avg = 0 → 不报警
    let burst = vec![("2026-06-15T10:00".to_string(), 200)];
    let out = apm_burst_from_data(&burst, 0.0);
    assert!(out.is_empty());
}

#[test]
fn apm_burst_above_3x_baseline_alerts() {
    // hist_avg = 1.0, 某分钟 200 → 200x
    let burst = vec![("2026-06-15T10:00".to_string(), 200)];
    let out = apm_burst_from_data(&burst, 1.0);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].kind, "apm_burst");
    assert_eq!(out[0].severity, "alert");
}

#[test]
fn apm_burst_below_multiplier_no_alert() {
    // hist_avg = 100, 某分钟 200 → 2x（< 3x 阈值）→ 不报警
    let burst = vec![("2026-06-15T10:00".to_string(), 200)];
    let out = apm_burst_from_data(&burst, 100.0);
    assert!(out.is_empty());
}

#[test]
fn apm_burst_empty_input_no_alert() {
    let out = apm_burst_from_data(&[], 50.0);
    assert!(out.is_empty());
}

// ─── 纯函数：marathon_from_minutes ────────────────────────────────────────────

#[test]
fn marathon_at_180_minutes_triggers() {
    // 10:00 - 12:59 共 180 连续分钟
    let mins: Vec<String> = (0..180)
        .map(|m| format!("2026-06-15T{:02}:{:02}", 10 + m / 60, m % 60))
        .collect();
    let out = marathon_from_minutes(&mins);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].kind, "marathon");
    assert!(out[0].message.contains("180"));
}

#[test]
fn marathon_below_threshold_no_alert() {
    // 只 100 连续分钟（< 180 阈值）
    let mins: Vec<String> = (0..100)
        .map(|m| format!("2026-06-15T10:{:02}", m))
        .collect();
    let out = marathon_from_minutes(&mins);
    assert!(out.is_empty());
}

#[test]
fn marathon_empty_no_alert() {
    let out = marathon_from_minutes(&[]);
    assert!(out.is_empty());
}

// ─── 纯函数：new_app_surge_from_data ──────────────────────────────────────────

#[test]
fn new_app_surge_first_time_app() {
    // 历史无该 app（hist_days=0），今天 60 事件 → 标记首次出现
    let today = vec![("NewApp.exe".to_string(), 60)];
    let out = new_app_surge_from_data(&today, |_| (0, 0));
    let surge = out
        .iter()
        .find(|a| a.kind == "new_app_surge")
        .expect("new_app_surge");
    assert!(surge.message.contains("NewApp.exe"));
    assert_eq!(surge.severity, "info");
}

#[test]
fn new_app_surge_steady_app_no_alert() {
    // 历史 200 事件 / 2 天 = 日均 100，今天 100 → 1x（< 5x）→ 不报警
    let today = vec![("Code.exe".to_string(), 100)];
    let out = new_app_surge_from_data(&today, |_| (200, 2));
    let surges: Vec<_> = out.iter().filter(|a| a.kind == "new_app_surge").collect();
    assert_eq!(surges.len(), 0, "steady app should not trigger");
}

#[test]
fn new_app_surge_above_5x_alerts() {
    // 历史 100 事件 / 10 天 = 日均 10，今天 60 → 6x（≥ 5x）→ 报警
    let today = vec![("Game.exe".to_string(), 60)];
    let out = new_app_surge_from_data(&today, |_| (100, 10));
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].kind, "new_app_surge");
    assert_eq!(out[0].severity, "warn");
}

#[test]
fn new_app_surge_small_app_ignored() {
    // 今天 < 50 事件 → 不参与判定
    let today = vec![("TinyApp.exe".to_string(), 40)];
    let out = new_app_surge_from_data(&today, |_| (0, 0));
    assert!(out.is_empty());
}

// ─── 编排层：detect_all（in-memory DB 端到端） ────────────────────────────────

fn setup() -> rusqlite::Connection {
    let conn = rusqlite::Connection::open_in_memory().expect("open in-memory");
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

#[test]
fn detect_all_empty_db_no_panic() {
    let conn = setup();
    let anomalies = kynoptic_core::anomaly::detect_all(&conn, "2026-06-15").unwrap();
    assert_eq!(anomalies.len(), 0);
}
