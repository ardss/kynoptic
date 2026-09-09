//! queries 查询函数集成测试
//!
//! 覆盖计数/范围查询的正确性，这些函数被 PetStateSync 线程和
//! SystemSnapshot::collect 在热路径中调用。

use chrono::Utc;
use kynoptic_core::db::Database;
use kynoptic_core::queries;
use kynoptic_core::types::{Event, EventAction, EventType};
use serde_json::json;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static SEQ: AtomicU64 = AtomicU64::new(0);

fn tmp_db_path() -> PathBuf {
    let seq = SEQ.fetch_add(1, Ordering::SeqCst);
    let mut p = std::env::temp_dir();
    // 路钥含纳秒级时间戳：进程 id 会被 Windows 快速复用，若上次运行遗留同
    // 名临时库（panic 跳过 cleanup / 删除时连接未关闭导致 delete-pending），
    // 仅 pid+seq 会在同日重跑时命中旧文件，造成计数翻倍 / UNIQUE 冲突假失败。
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64 + d.as_secs() * 1_000_000_000)
        .unwrap_or(0);
    p.push(format!(
        "dp_qtest_{}_{}_{}.db",
        std::process::id(),
        seq,
        nanos
    ));
    p
}

fn fresh_db() -> (Database, PathBuf) {
    let path = tmp_db_path();
    let db = Database::open(path.to_str().unwrap()).expect("数据库打开失败");
    (db, path)
}

fn cleanup(path: &PathBuf) {
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(format!("{}-wal", path.display()));
    let _ = std::fs::remove_file(format!("{}-shm", path.display()));
}

/// 直接插入一条带指定时间戳/类型/动作的事件（绕过 Event::new 的 Utc::now）
fn insert_event(db: &Database, ts: &str, etype: &str, eaction: &str, app: Option<&str>) {
    let conn = db.reader();
    // 借用读连接无法写，这里用 metadata 的写路径临时绕过：
    // 直接通过底层 writer 不公开，改用 insert_events + 手动 timestamp。
    drop(conn);
    let mut e = Event::new(
        match eaction {
            "press" => EventAction::Press,
            "click" => EventAction::Click,
            "switch" => EventAction::Switch,
            "heartbeat" => EventAction::Heartbeat,
            _ => EventAction::Heartbeat,
        },
        match etype {
            "keyboard" => EventType::Keyboard,
            "mouse" => EventType::Mouse,
            "window" => EventType::Window,
            "system" => EventType::System,
            _ => EventType::System,
        },
    );
    e.timestamp = ts.to_string();
    if let Some(a) = app {
        e.app_name = Some(a.to_string());
    }
    db.insert_events(&[e]);
}

fn now_rfc3339() -> String {
    Utc::now().to_rfc3339()
}

// === today 范围查询 ===

#[test]
fn count_today_keys_counts_only_today_presses() {
    let (db, path) = fresh_db();
    let now = now_rfc3339();
    let today = Utc::now().format("%Y-%m-%d").to_string();
    let tomorrow = (Utc::now() + chrono::Duration::days(1))
        .format("%Y-%m-%d")
        .to_string();

    // 今日 3 次按键 + 1 次点击（点击不计入 keys）
    insert_event(&db, &now, "keyboard", "press", None);
    insert_event(&db, &now, "keyboard", "press", None);
    insert_event(&db, &now, "keyboard", "press", None);
    insert_event(&db, &now, "mouse", "click", None);

    let conn = db.reader();
    assert_eq!(queries::count_today_keys(&conn, &today, &tomorrow), 3);
    cleanup(&path);
}

#[test]
fn count_today_clicks_excludes_keys() {
    let (db, path) = fresh_db();
    let now = now_rfc3339();
    let today = Utc::now().format("%Y-%m-%d").to_string();
    let tomorrow = (Utc::now() + chrono::Duration::days(1))
        .format("%Y-%m-%d")
        .to_string();

    insert_event(&db, &now, "mouse", "click", None);
    insert_event(&db, &now, "mouse", "click", None);
    insert_event(&db, &now, "keyboard", "press", None);

    let conn = db.reader();
    assert_eq!(queries::count_today_clicks(&conn, &today, &tomorrow), 2);
    cleanup(&path);
}

#[test]
fn today_range_format() {
    use chrono::{Local, TimeZone, Utc};
    let (today, tomorrow) = queries::today_range();
    // today_range 返回本地「今日」的 UTC RFC3339 边界：[本地今日午夜, 本地明日午夜)，
    // 转成 UTC。验证三件事：1) today 是本地今日午夜对应的 UTC 时刻；2) tomorrow 是今日+1天；
    // 3) 区间恰好 24 小时（86400 秒）。
    let local_today = Local::now().format("%Y-%m-%d").to_string();
    let expected_today = Local::now()
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .and_then(|n| {
            Local
                .from_local_datetime(&n)
                .earliest()
                .map(|dt| dt.with_timezone(&Utc).to_rfc3339())
        });
    assert_eq!(
        Some(today.clone()),
        expected_today,
        "today_range 的 start 应为本地 {} 午夜的 UTC RFC3339",
        local_today
    );
    assert!(tomorrow > today, "tomorrow 应大于 today");
    // 区间恰好 24 小时
    let start = chrono::DateTime::parse_from_rfc3339(&today).unwrap();
    let end = chrono::DateTime::parse_from_rfc3339(&tomorrow).unwrap();
    let secs = (end - start).num_seconds();
    assert_eq!(secs, 86400, "今日区间应为 86400 秒，实际 {secs}");
    // 与 local_day_range 一致
    let (ls, le) = queries::local_day_range(&local_today).expect("local_day_range 解析本地今日");
    assert_eq!(ls, today);
    assert_eq!(le, tomorrow);
}

#[test]
fn count_today_events_all_types() {
    let (db, path) = fresh_db();
    let now = now_rfc3339();
    let today = Utc::now().format("%Y-%m-%d").to_string();
    let tomorrow = (Utc::now() + chrono::Duration::days(1))
        .format("%Y-%m-%d")
        .to_string();

    insert_event(&db, &now, "keyboard", "press", None);
    insert_event(&db, &now, "mouse", "click", None);
    insert_event(&db, &now, "system", "heartbeat", None);

    let conn = db.reader();
    assert_eq!(queries::count_today_events(&conn, &today, &tomorrow), 3);
    cleanup(&path);
}

// === current_app / window ===

#[test]
fn current_app_returns_latest_window_event() {
    let (db, path) = fresh_db();
    let now = now_rfc3339();
    let today = Utc::now().format("%Y-%m-%d").to_string();
    let tomorrow = (Utc::now() + chrono::Duration::days(1))
        .format("%Y-%m-%d")
        .to_string();

    insert_event(&db, &now, "window", "switch", Some("chrome"));
    // 稍晚一条
    let later = (Utc::now() + chrono::Duration::seconds(1)).to_rfc3339();
    insert_event(&db, &later, "window", "switch", Some("code"));

    let conn = db.reader();
    assert_eq!(queries::current_app(&conn, &today, &tomorrow), "code");
    cleanup(&path);
}

#[test]
fn current_app_empty_when_no_window_events() {
    let (db, path) = fresh_db();
    let today = Utc::now().format("%Y-%m-%d").to_string();
    let tomorrow = (Utc::now() + chrono::Duration::days(1))
        .format("%Y-%m-%d")
        .to_string();
    let conn = db.reader();
    assert_eq!(queries::current_app(&conn, &today, &tomorrow), "");
    cleanup(&path);
}

// === all-time 计数 ===

#[test]
fn count_all_keys_clicks_total() {
    let (db, path) = fresh_db();
    let now = now_rfc3339();
    let yesterday = (Utc::now() - chrono::Duration::days(1)).to_rfc3339();

    insert_event(&db, &now, "keyboard", "press", None);
    insert_event(&db, &yesterday, "keyboard", "press", None);
    insert_event(&db, &now, "mouse", "click", None);

    let conn = db.reader();
    assert_eq!(queries::count_all_keys(&conn), 2);
    assert_eq!(queries::count_all_clicks(&conn), 1);
    cleanup(&path);
}

// === since 增量查询（PetStateSync 线程核心）===

#[test]
fn count_keys_since_respects_id_boundary() {
    let (db, path) = fresh_db();
    let now = now_rfc3339();
    insert_event(&db, &now, "keyboard", "press", None);
    insert_event(&db, &now, "keyboard", "press", None);
    insert_event(&db, &now, "keyboard", "press", None);

    let conn = db.reader();
    let max_id = queries::max_event_id(&conn).unwrap();
    // 3 条都在 id <= max_id，since=max_id 后应计 0
    assert_eq!(queries::count_keys_since(&conn, max_id), 0);
    // since=0 计全部
    assert_eq!(queries::count_keys_since(&conn, 0), 3);
    drop(conn);

    // 再插一条
    insert_event(&db, &now, "keyboard", "press", None);
    let conn = db.reader();
    // 自上次 max_id 后新增 1 条
    assert_eq!(queries::count_keys_since(&conn, max_id), 1);
    cleanup(&path);
}

#[test]
fn max_event_id_none_when_empty() {
    let (db, path) = fresh_db();
    let conn = db.reader();
    assert_eq!(queries::max_event_id(&conn), None);
    cleanup(&path);
}

#[test]
fn keys_clicks_since_matches_individual() {
    // 等价性守护：keys_clicks_since 与 count_keys_since + count_clicks_since 完全一致
    let (db, path) = fresh_db();
    let now = now_rfc3339();
    // 混合插入：3 键 + 2 点击 + 其他
    insert_event(&db, &now, "keyboard", "press", None);
    insert_event(&db, &now, "keyboard", "press", None);
    insert_event(&db, &now, "keyboard", "press", None);
    insert_event(&db, &now, "mouse", "click", None);
    insert_event(&db, &now, "mouse", "click", None);
    insert_event(&db, &now, "mouse", "scroll", None); // 不计入 keys/clicks

    let conn = db.reader();
    let max_id = queries::max_event_id(&conn).unwrap();

    // since=0：全量
    let (k, c) = queries::keys_clicks_since(&conn, 0);
    assert_eq!(k, queries::count_keys_since(&conn, 0));
    assert_eq!(c, queries::count_clicks_since(&conn, 0));
    assert_eq!((k, c), (3, 2));

    // since=max_id：无新增
    let (k, c) = queries::keys_clicks_since(&conn, max_id);
    assert_eq!(
        (k, c),
        (
            queries::count_keys_since(&conn, max_id),
            queries::count_clicks_since(&conn, max_id)
        )
    );
    assert_eq!((k, c), (0, 0));
    drop(conn);

    // 再插一条按键，验证增量
    insert_event(&db, &now, "keyboard", "press", None);
    let conn = db.reader();
    let (k, c) = queries::keys_clicks_since(&conn, max_id);
    assert_eq!(
        (k, c),
        (
            queries::count_keys_since(&conn, max_id),
            queries::count_clicks_since(&conn, max_id)
        )
    );
    assert_eq!((k, c), (1, 0));
    cleanup(&path);
}

#[test]
fn keys_clicks_since_empty_db_returns_zeros() {
    let (db, path) = fresh_db();
    let conn = db.reader();
    // 无事件，SUM 返回 NULL → 转 0
    assert_eq!(queries::keys_clicks_since(&conn, 0), (0, 0));
    assert_eq!(queries::keys_clicks_since(&conn, 999), (0, 0));
    cleanup(&path);
}

// === active minutes 去重 ===

#[test]
fn count_all_active_minutes_dedupes_same_minute() {
    let (db, path) = fresh_db();
    let now = now_rfc3339();
    // 同一分钟内多次按键只算一个活跃分钟
    insert_event(&db, &now, "keyboard", "press", None);
    insert_event(&db, &now, "keyboard", "press", None);
    insert_event(&db, &now, "mouse", "click", None);

    let conn = db.reader();
    // 同一 timestamp → substr(1,16) 相同 → 去重为 1
    assert_eq!(queries::count_all_active_minutes(&conn), 1);
    cleanup(&path);
}

// === recent input ===

#[test]
fn count_recent_input_window() {
    let (db, path) = fresh_db();
    let now = now_rfc3339();
    let old = (Utc::now() - chrono::Duration::seconds(600)).to_rfc3339();

    insert_event(&db, &now, "keyboard", "press", None);
    insert_event(&db, &old, "keyboard", "press", None);

    let conn = db.reader();
    // 最近 60s 窗口内只有 1 条
    let one_min_ago = (Utc::now() - chrono::Duration::seconds(60)).to_rfc3339();
    assert_eq!(queries::count_recent_input(&conn, &one_min_ago), 1);
    cleanup(&path);
}

// === latest_event_data ===

/// 插入一条带指定时间戳/类型/动作/JSON 数据的系统事件
fn insert_sys_event(
    db: &Database,
    ts: &str,
    eaction: EventAction,
    etype: EventType,
    data: serde_json::Value,
) {
    let mut e = Event::new(eaction, etype);
    e.timestamp = ts.to_string();
    e.event_data = Some(data);
    db.insert_events(&[e]);
}

#[test]
fn latest_event_data_parses_json() {
    let (db, path) = fresh_db();
    let mut e = Event::new(EventAction::Heartbeat, EventType::System);
    e.timestamp = now_rfc3339();
    e.event_data = Some(serde_json::json!({
        "cpu_percent": 75.5,
        "memory": {"used_percent": 60.0}
    }));
    db.insert_events(&[e]);

    let conn = db.reader();
    let data = queries::latest_event_data(&conn, EventType::System, EventAction::Heartbeat);
    assert!(data.is_some());
    let d = data.unwrap();
    assert_eq!(d["cpu_percent"].as_f64(), Some(75.5));
    assert_eq!(d["memory"]["used_percent"].as_f64(), Some(60.0));
    cleanup(&path);
}

#[test]
fn latest_event_data_none_when_no_match() {
    let (db, path) = fresh_db();
    let conn = db.reader();
    // 空 db：任何 (type, action) 都查不到
    assert_eq!(
        queries::latest_event_data(&conn, EventType::System, EventAction::Heartbeat),
        None
    );
    cleanup(&path);
}

// === latest_event_data_multi 与逐条 latest_event_data 的等价性（热路径优化守护）===

#[test]
fn latest_event_data_multi_matches_individual() {
    let (db, path) = fresh_db();
    let t_old = (Utc::now() - chrono::Duration::seconds(100)).to_rfc3339();
    let t_new = now_rfc3339();

    // 每个组合插入多条，验证取最新非 NULL 行
    // heartbeat: 旧(75.5) → 新(90.0)，应取 90.0
    insert_sys_event(
        &db,
        &t_old,
        EventAction::Heartbeat,
        EventType::System,
        json!({"cpu_percent": 75.5}),
    );
    insert_sys_event(
        &db,
        &t_new,
        EventAction::Heartbeat,
        EventType::System,
        json!({"cpu_percent": 90.0}),
    );
    // thermal_snapshot: 仅一条
    insert_sys_event(
        &db,
        &t_new,
        EventAction::ThermalSnapshot,
        EventType::System,
        json!({"max_temp_celsius": 65.0}),
    );
    // wifi_change: 旧(connected=true) → 新(connected=false)，应取 false
    insert_sys_event(
        &db,
        &t_old,
        EventAction::WifiChange,
        EventType::Network,
        json!({"connected": true}),
    );
    insert_sys_event(
        &db,
        &t_new,
        EventAction::WifiChange,
        EventType::Network,
        json!({"connected": false}),
    );

    let targets = [
        ("system", "heartbeat"),
        ("system", "thermal_snapshot"),
        ("system", "gpu_snapshot"), // 无数据
        ("network", "wifi_change"),
    ];
    let conn = db.reader();
    let multi = queries::latest_event_data_multi(&conn, &targets);

    // 逐条对比（用枚举调用 latest_event_data）
    let single =
        |etype: EventType, eaction: EventAction| queries::latest_event_data(&conn, etype, eaction);

    // heartbeat
    let m = multi
        .get(&("system".into(), "heartbeat".into()))
        .expect("heartbeat 应存在");
    assert_eq!(m["cpu_percent"].as_f64(), Some(90.0)); // 取最新
    assert_eq!(
        m,
        &single(EventType::System, EventAction::Heartbeat).unwrap()
    );

    // thermal
    let m = multi
        .get(&("system".into(), "thermal_snapshot".into()))
        .unwrap();
    assert_eq!(m["max_temp_celsius"].as_f64(), Some(65.0));
    assert_eq!(
        m,
        &single(EventType::System, EventAction::ThermalSnapshot).unwrap()
    );

    // wifi（取最新 connected=false）
    let m = multi
        .get(&("network".into(), "wifi_change".into()))
        .unwrap();
    assert_eq!(m["connected"].as_bool(), Some(false));
    assert_eq!(
        m,
        &single(EventType::Network, EventAction::WifiChange).unwrap()
    );

    // gpu_snapshot 无数据：multi 不含该 key，single 返回 None
    assert!(!multi.contains_key(&("system".into(), "gpu_snapshot".into())));
    assert!(single(EventType::System, EventAction::GpuSnapshot).is_none());
    cleanup(&path);
}

#[test]
fn latest_event_data_multi_empty_targets() {
    let (db, path) = fresh_db();
    let conn = db.reader();
    let multi = queries::latest_event_data_multi(&conn, &[]);
    assert!(multi.is_empty());
    cleanup(&path);
}

#[test]
fn latest_event_data_multi_empty_db() {
    let (db, path) = fresh_db();
    let conn = db.reader();
    let multi = queries::latest_event_data_multi(
        &conn,
        &[("system", "heartbeat"), ("network", "wifi_change")],
    );
    assert!(multi.is_empty());
    cleanup(&path);
}

#[test]
fn latest_event_data_multi_skips_null_event_data() {
    // event_data 为 NULL 的行应被跳过，取最新的非 NULL 行
    let (db, path) = fresh_db();
    let t_new = now_rfc3339();
    let t_old = (Utc::now() - chrono::Duration::seconds(100)).to_rfc3339();

    // 最新行 event_data 为 NULL，旧行有数据 → 应取旧行数据
    let mut e_null = Event::new(EventAction::Heartbeat, EventType::System);
    e_null.timestamp = t_new;
    e_null.event_data = None; // NULL
    db.insert_events(&[e_null]);
    insert_sys_event(
        &db,
        &t_old,
        EventAction::Heartbeat,
        EventType::System,
        json!({"cpu_percent": 42.0}),
    );

    let conn = db.reader();
    let multi = queries::latest_event_data_multi(&conn, &[("system", "heartbeat")]);
    let m = multi.get(&("system".into(), "heartbeat".into())).unwrap();
    assert_eq!(m["cpu_percent"].as_f64(), Some(42.0));

    // 与单条 latest_event_data 一致（它也过滤 IS NOT NULL）
    let single =
        queries::latest_event_data(&conn, EventType::System, EventAction::Heartbeat).unwrap();
    assert_eq!(m, &single);
    cleanup(&path);
}

#[test]
fn yesterday_total_same_time_counts_keyboard_mouse() {
    let (db, path) = fresh_db();
    // 昨日同一时刻之前的键鼠事件
    let yesterday = (Utc::now() - chrono::Duration::days(1)).to_rfc3339();
    let mk = |action: EventAction, etype: EventType| {
        let mut e = Event::new(action, etype);
        e.timestamp = yesterday.clone();
        e
    };
    db.insert_events(&[
        mk(EventAction::Press, EventType::Keyboard),
        mk(EventAction::Click, EventType::Mouse),
        mk(EventAction::Press, EventType::Keyboard),
    ]);

    let conn = db.reader();
    // 昨日截至此刻的键鼠交互总数 = 3
    assert_eq!(queries::yesterday_total_same_time(&conn), 3);
    cleanup(&path);
}

// === today_action_breakdown 与逐条查询的等价性（热路径优化守护）===

#[test]
fn today_action_breakdown_matches_individual_counts() {
    let (db, path) = fresh_db();
    let now = now_rfc3339();
    let today = Utc::now().format("%Y-%m-%d").to_string();
    let tomorrow = (Utc::now() + chrono::Duration::days(1))
        .format("%Y-%m-%d")
        .to_string();

    // 混合插入各类今日事件
    insert_event(&db, &now, "keyboard", "press", None);
    insert_event(&db, &now, "keyboard", "press", None);
    insert_event(&db, &now, "keyboard", "press", None);
    insert_event(&db, &now, "keyboard", "release", None);
    insert_event(&db, &now, "mouse", "click", None);
    insert_event(&db, &now, "mouse", "click", None);
    insert_event(&db, &now, "mouse", "scroll", None);
    insert_event(&db, &now, "mouse", "move", None);
    insert_event(&db, &now, "window", "switch", Some("code"));

    let conn = db.reader();
    let (total, counts) = queries::today_action_breakdown(&conn, &today, &tomorrow);

    // total 等价于 count_today_events
    assert_eq!(total, queries::count_today_events(&conn, &today, &tomorrow));
    assert_eq!(total, 9);

    // 各组合计数等价于逐条 count_today_action / count_today_keys / count_today_clicks
    let cnt = |t: &str, a: &str| counts.get(&(t.into(), a.into())).copied().unwrap_or(0);
    assert_eq!(
        cnt("keyboard", "press"),
        queries::count_today_keys(&conn, &today, &tomorrow)
    );
    assert_eq!(
        cnt("mouse", "click"),
        queries::count_today_clicks(&conn, &today, &tomorrow)
    );
    assert_eq!(
        cnt("mouse", "scroll"),
        queries::count_today_action(&conn, "mouse", "scroll", &today, &tomorrow)
    );
    assert_eq!(
        cnt("keyboard", "release"),
        queries::count_today_action(&conn, "keyboard", "release", &today, &tomorrow)
    );
    assert_eq!(
        cnt("mouse", "move"),
        queries::count_today_action(&conn, "mouse", "move", &today, &tomorrow)
    );
    assert_eq!(
        cnt("window", "switch"),
        queries::count_today_action(&conn, "window", "switch", &today, &tomorrow)
    );

    // 具体值
    assert_eq!(cnt("keyboard", "press"), 3);
    assert_eq!(cnt("mouse", "click"), 2);
    assert_eq!(cnt("window", "switch"), 1);
    cleanup(&path);
}

#[test]
fn today_action_breakdown_empty_when_no_events() {
    let (db, path) = fresh_db();
    let today = Utc::now().format("%Y-%m-%d").to_string();
    let tomorrow = (Utc::now() + chrono::Duration::days(1))
        .format("%Y-%m-%d")
        .to_string();

    let conn = db.reader();
    let (total, counts) = queries::today_action_breakdown(&conn, &today, &tomorrow);
    assert_eq!(total, 0);
    assert!(counts.is_empty());
    cleanup(&path);
}

// === 时区切日（本地 vs UTC） ===

#[test]
fn local_day_range_is_24h_and_self_consistent() {
    // 任意一个本地日期，返回区间应跨度恰好 24 小时、start < end。
    let (start, end) = queries::local_day_range("2026-06-20").expect("解析日期");
    let s = chrono::DateTime::parse_from_rfc3339(&start).unwrap();
    let e = chrono::DateTime::parse_from_rfc3339(&end).unwrap();
    assert!(start < end);
    assert_eq!((e - s).num_seconds(), 86400);
}

#[test]
fn local_day_range_rejects_garbage() {
    assert!(queries::local_day_range("not-a-date").is_none());
    assert!(queries::local_day_range("2026-13-40").is_none());
}

#[test]
fn today_range_captures_event_at_local_midnight() {
    // 核心回归守卫：用 today_range 得到的本地今日起点插一条事件，
    // 它必须被「今日」计数命中——证明切日边界用的是本地时区而非 UTC。
    let (db, path) = fresh_db();
    let (today, tomorrow) = queries::today_range();
    // 在今日起点 +1 秒处插入（落在本地今日凌晨；若按 UTC 切日，此刻可能仍属昨天）。
    let start = chrono::DateTime::parse_from_rfc3339(&today).unwrap();
    let ts = (start + chrono::Duration::seconds(1)).to_rfc3339();
    insert_event(&db, &ts, "keyboard", "press", None);

    let conn = db.reader();
    assert_eq!(
        queries::count_today_keys(&conn, &today, &tomorrow),
        1,
        "本地今日起点附近的事件应被计入今日"
    );
    cleanup(&path);
}

#[test]
fn hourly_buckets_are_local_hours() {
    // 插入本地今日起点 +13 小时的事件，hourly 投影应落在「本地 13 时」桶，
    // 而非 UTC 小时。验证 datetime(timestamp, offset) 的时区换算生效。
    let (db, path) = fresh_db();
    let (today, tomorrow) = queries::today_range();
    let start = chrono::DateTime::parse_from_rfc3339(&today).unwrap();
    let ts = (start + chrono::Duration::hours(13) + chrono::Duration::minutes(5)).to_rfc3339();
    insert_event(&db, &ts, "keyboard", "press", None);

    let conn = db.reader();
    let hourly = queries::hourly_counts_today(&conn, &today, &tomorrow);
    assert_eq!(hourly.len(), 1, "应只有一个小时桶");
    assert_eq!(
        hourly[0],
        (13, 1),
        "事件应落在本地 13 时桶，实际 {:?}",
        hourly[0]
    );
    cleanup(&path);
}
