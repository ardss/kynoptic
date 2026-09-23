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
    // 10:00 - 12:59 共 180 连续分钟（bridge=0，不桥接）
    let mins: Vec<String> = (0..180)
        .map(|m| format!("2026-06-15T{:02}:{:02}", 10 + m / 60, m % 60))
        .collect();
    let out = marathon_from_minutes(&mins, 0);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].kind, "marathon");
    assert!(out[0].message.contains("180"));
}

#[test]
fn marathon_below_threshold_no_alert() {
    // 只 100 连续分钟（< 180 阈值），即使桥接也不该报
    let mins: Vec<String> = (0..100)
        .map(|m| format!("2026-06-15T10:{:02}", m))
        .collect();
    let out = marathon_from_minutes(&mins, 15);
    assert!(out.is_empty());
}

#[test]
fn marathon_empty_no_alert() {
    let out = marathon_from_minutes(&[], 2);
    assert!(out.is_empty());
}

/// 口径（统一 2026-09）：90 + 6 分钟午饭空洞 + 90（由 85+6gap+85 原型扩到
/// 90/90 以越过 180 阈值）——bridge=2 时午饭把会话拆开（最长 90，不报）；
/// bridge=15 时空洞按"无输入阅读"补齐（连续 186，报马拉松）。
#[test]
fn marathon_bridge_gap_2_vs_15_split_or_merge() {
    let mut mins: Vec<String> = Vec::new();
    // 09:00-10:29（90 分钟）
    for m in 0..90 {
        mins.push(format!("2026-06-15T{:02}:{:02}", 9 + (m / 60), m % 60));
    }
    // 10:30-10:35 缺席（6 分钟午饭），10:36-12:05（90 分钟）
    for m in 36..126 {
        mins.push(format!("2026-06-15T{:02}:{:02}", 10 + (m / 60), m % 60));
    }
    // bridge=2：6 分钟间隙 > 2，不桥接 → 最长 90 < 180，不报
    let out2 = marathon_from_minutes(&mins, 2);
    assert!(out2.is_empty(), "bridge=2 时午饭应拆开会话");
    // bridge=15：6 分钟间隙被补齐 → 连续 90+6+90=186，报马拉松
    let out15 = marathon_from_minutes(&mins, 15);
    assert_eq!(out15.len(), 1, "bridge=15 时午饭应被桥接");
    assert!(out15[0].message.contains("186"), "{}", out15[0].message);
}

/// 交叉审查 P2：马拉松起止时间必须按**本地时区**格式化——此前用 UTC，
/// 本地 12:00 开始的马拉松在异常卡上显示为 UTC 04:00。
#[test]
fn marathon_time_range_is_local_not_utc() {
    // 锚定当前时刻的整分钟，构造 180 连续分钟
    use chrono::Timelike;
    let start = chrono::Utc::now()
        .with_second(0)
        .unwrap()
        .with_nanosecond(0)
        .unwrap();
    let m0 = start.timestamp() / 60;
    let mins: Vec<String> = (0..180)
        .map(|i| {
            chrono::DateTime::from_timestamp((m0 + i) * 60, 0)
                .unwrap()
                .with_timezone(&chrono::Local)
                .format("%Y-%m-%dT%H:%M")
                .to_string()
        })
        .collect();
    let out = marathon_from_minutes(&mins, 0);
    assert_eq!(out.len(), 1);
    let expect_start = start
        .with_timezone(&chrono::Local)
        .format("%Y-%m-%dT%H:%M")
        .to_string();
    let expect_end = (start + chrono::Duration::minutes(179))
        .with_timezone(&chrono::Local)
        .format("%Y-%m-%dT%H:%M")
        .to_string();
    assert!(
        out[0]
            .detail
            .contains(&format!("从 {expect_start} 到 {expect_end}")),
        "起止时间必须是本地时区: {}",
        out[0].detail
    );
    assert_eq!(out[0].at.as_deref(), Some(expect_start.as_str()));
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

// ─── 编排层：marathon 与 presence 同源（剔注入；事件路径，时区无关） ──────────

fn setup_events() -> rusqlite::Connection {
    // 与纯函数测试同一份 events 表结构（classify_minutes 只读 events）
    setup()
}

/// 往 `date`（本地日）的第 `min_of_day` 分钟插一条 input_agg 事件（本地时区无关：
/// 从 local_day_range 的 UTC 起点推时刻，与 charts.rs 测试同模式）。
fn ins_minute_event(c: &rusqlite::Connection, date: &str, min_of_day: usize, data: &str) {
    let (start, _) = kynoptic_core::queries::local_day_range(date).unwrap();
    let base = chrono::DateTime::parse_from_rfc3339(&start).unwrap();
    let t = base + chrono::Duration::minutes(min_of_day as i64);
    c.execute(
        "INSERT INTO events (timestamp, event_type, event_action, event_data, session_id) \
         VALUES (?1, 'keyboard', 'input_agg', ?2, 1)",
        rusqlite::params![t.to_rfc3339(), data],
    )
    .unwrap();
}

/// 口径修复回归（2026-09）：marathon 必须与 presence 同源——
/// - 纯人类输入 180 分钟（09:30 为 move-only 空洞）：bridge=0 断开不报，
///   bridge=15 桥接报 180；
/// - 同样 180 分钟全是**注入**输入：presence 判 auto、human=0，不得报马拉松
///   （旧实现读 input_keys 桶会误报「连续在场 120/180 分钟」）。
#[test]
fn detect_marathon_uses_human_minutes_injected_excluded() {
    let conn = setup_events();
    let date = "2026-06-15";
    // 08:00-10:59 共 180 分钟；570（09:30）换成 move-only（纯鼠标移动）
    for m in 480..660 {
        if m == 570 {
            ins_minute_event(&conn, date, m, r#"{"moves":9,"samples":9}"#);
        } else {
            ins_minute_event(&conn, date, m, r#"{"keys":4,"samples":4}"#);
        }
    }
    // bridge=0：move-only 分钟剔除后空洞断开，不报马拉松
    let out0 = kynoptic_core::anomaly::detect_marathon_session(&conn, date, 0).unwrap();
    assert!(out0.is_empty(), "move-only 分钟不能撑起马拉松");
    // bridge=15：1 分钟空洞按无输入阅读补齐 → 连续 180，报马拉松
    let out15 = kynoptic_core::anomaly::detect_marathon_session(&conn, date, 15).unwrap();
    assert_eq!(out15.len(), 1);
    assert!(out15[0].message.contains("180"), "{}", out15[0].message);

    // 注入输入不算人在场：清空后重插同样的 180 分钟，但全为 injected_keys
    conn.execute("DELETE FROM events", []).unwrap();
    for m in 480..660 {
        ins_minute_event(
            &conn,
            date,
            m,
            r#"{"keys":120,"injected_keys":120,"samples":120}"#,
        );
    }
    let d = kynoptic_core::queries::classify_minutes(&conn, date, 15);
    assert_eq!(d.presence_minutes, 0, "纯注入输入人在场必须为 0");
    assert_eq!(d.automation_minutes, 180);
    let out_auto = kynoptic_core::anomaly::detect_marathon_session(&conn, date, 15).unwrap();
    assert!(
        out_auto.is_empty(),
        "注入输入不得伪造马拉松: {:?}",
        out_auto.iter().map(|a| &a.message).collect::<Vec<_>>()
    );
}
