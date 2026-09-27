//! 聚合读缓存（agg_minute / agg_daily）集成测试
//!
//! 覆盖：懒回填幂等、增量维护与全量重建等价、**原始 events 不可动**
//! （数量 + 内容指纹不变）、缓存路径与 events 现算路径结果同解、
//! input_agg 计数行对下游计数查询的兼容。
//!
//! 时钟红线（CONTRIBUTING §4）：fixture 时间一律由注入锚点派生（本地今日
//! 日期的正午 12:00，now+1min 永不跨本地午夜），不读真实时钟、不依赖
//! 本地时区时刻——23:59–00:00 窗口跑 cargo test 不再 flake。

use chrono::{NaiveDate, TimeZone};
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
    // 红线：路径键不读真实时钟（旧实现放 SystemTime 纳秒）。纯 pid+seq
    // （AtomicU64 计数）；上次运行遗留同名临时库（panic 跳过清理/删库时
    // 连接未关闭导致 delete-pending）则递增 seq 直到取到空名。
    let mut seq = SEQ.fetch_add(1, Ordering::SeqCst);
    loop {
        let mut p = std::env::temp_dir();
        p.push(format!("dp_aggtest_{}_{}.db", std::process::id(), seq));
        if !p.exists() {
            return p;
        }
        seq = SEQ.fetch_add(1, Ordering::SeqCst);
    }
}

/// 冻结锚点（红线：fixture 不读真实时钟）：`date` 所指本地日期的本地正午
/// 12:00 → UTC 时刻。now=12:00、agg=12:01 都严格落在今日本地区间
/// [00:00, 次日 00:00) 内，now+1min 永不跨本地午夜。约定与
/// midnight_bucket_test 一致：日期只取一次锚点（today_local_str），时刻
/// 手工定死。
fn noon_anchor_utc(date: &NaiveDate) -> chrono::DateTime<chrono::Utc> {
    let naive = date.and_hms_opt(12, 0, 0).expect("本地正午 12:00 构造失败");
    chrono::Local
        .from_local_datetime(&naive)
        .earliest()
        .expect("本地正午 12:00 不存在（DST 跳变日请换锚点日期）")
        .with_timezone(&chrono::Utc)
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
    // 红线：fixture 时间全部由注入锚点派生——日期只取一次 today_local_str
    // 作锚，「明日/昨今」同日推得，now=本地正午、agg=+1min，不再各自读
    // Utc::now()（旧实现在本地 23:59 窗口内 now+1min 跨本地午夜，agg 行
    // 落到 D+1，今日区间计数假红）。
    let today = queries::today_local_str();
    let date = NaiveDate::parse_from_str(&today, "%Y-%m-%d").expect("today 必须为本地日期");
    let tomorrow = date
        .succ_opt()
        .expect("无法推明日日期")
        .format("%Y-%m-%d")
        .to_string();
    let (start, end) = queries::local_day_range(&today).unwrap();
    let now = noon_anchor_utc(&date);
    let now_ts = now.to_rfc3339();

    // raw 行与 agg 行分属不同分钟：两种粒度模式互斥，同分钟只有一种形态
    // （agg 行代表整分钟总量）。agg 行本身用 MAX 快照语义防秒级累计膨胀。
    let agg_minute = (now + chrono::Duration::minutes(1)).to_rfc3339();
    let events = vec![
        raw(&now_ts, "keyboard", "press", None), // 1 raw key
        minute_agg_row(&agg_minute, "keyboard", json!({"keys": 5, "samples": 5})), // +5 agg keys
        raw(&now_ts, "mouse", "click", None),    // 1 raw click
        minute_agg_row(
            &agg_minute,
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
    db.update_agg(&events, &(1..=events.len() as i64).collect::<Vec<i64>>());
    // 异常读取走 agg 缓存路径，数值与 events 现算一致
    let conn = db.reader();
    assert!(agg::has_minute_for_date(&conn, &today));
    assert_eq!(queries::late_night_key_count(&conn, &today, 0, 0), 6); // hour>=0 = 全天
    assert_eq!(queries::day_totals(&conn, &today).unwrap().keys, 6);
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
    db.update_agg(&events, &(1..=events.len() as i64).collect::<Vec<i64>>());
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
    // 冻结锚点：now = 本地正午（严格在今日本地区间内）；近 200 分钟窗口
    // 为本地 08:40–12:00，全部落在「今日」，不跨午夜也不含未来时刻。
    let today = queries::today_local_str();
    let date = NaiveDate::parse_from_str(&today, "%Y-%m-%d").expect("today 必须为本地日期");
    let yesterday = date.pred_opt().expect("无法推昨日日期");
    let now = noon_anchor_utc(&date);

    // 近 200 分钟逐分钟 1 次按键（马拉松形态）+ 正午分钟 300 键突增 + app 事件
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

    // APM 基线（history），使 apm_burst 具备触发条件；基线日期由锚点日
    // 直接推得（不再二次读时钟）
    db.with_writer(
        |conn| {
            conn.execute(
                "INSERT INTO daily_agg (date, keys, clicks, active_minutes, apm_avg)
                 VALUES (?1, 1, 1, 1, 1.0)",
                // 基线必须是"历史"日期，daily_agg_avg_apm_before 只读 date < today
                rusqlite::params![yesterday.format("%Y-%m-%d").to_string()],
            )
            .unwrap();
        },
        || panic!("写连接不可用"),
    );

    // —— 无缓存（events 现算）基准值 ——
    let legacy_day = queries::day_totals(db.reader().deref(), &today).unwrap();
    let legacy_late_night = queries::late_night_key_count(db.reader().deref(), &today, 0, 0);
    let legacy_burst_max = queries::top_burst_minutes(db.reader().deref(), &today, 100)
        .iter()
        .map(|(_, n)| *n)
        .max()
        .unwrap_or(0);

    // —— 建缓存后（agg 读路径）——
    db.update_agg(&events, &(1..=events.len() as i64).collect::<Vec<i64>>());
    let conn = db.reader();
    let cached_day = queries::day_totals(&conn, &today).unwrap();
    let cached_late_night = queries::late_night_key_count(&conn, &today, 0, 0);
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
        let _ = kynoptic_core::db::run_migrations(&conn);
        conn.execute(
            "INSERT INTO events (timestamp, event_type, event_action, event_data, app_name, window_title, session_id)
             VALUES ('2026-06-15T12:00:00+00:00', 'keyboard', 'press', NULL, NULL, NULL, 1)",
            [],
        )
        .unwrap();
    }
    let db = Database::open(path.to_str().unwrap()).unwrap();
    let conn = db.reader();
    // 回填在后台线程分块执行（perf3：不阻塞 open）。用完成信号同步等待，
    // 而不是盲轮询——根治竞态。
    assert!(
        db.wait_for_backfill(std::time::Duration::from_secs(15)),
        "首次 open 应触发后台懒回填并在超时前完成"
    );
    assert!(
        agg::has_minute_for_date(&conn, "2026-06-15"),
        "回填完成后该日应有 agg_minute 行"
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
    db.update_agg(&events, &(1..=events.len() as i64).collect::<Vec<i64>>());
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

/// 审查 P1 自愈回归：模拟"插入事件后、agg 更新前"的 kill 中断（直接往 events
/// 插原始贡献行、不更新 agg），重开 Database 时欠聚合核对必须发现并自动重算，
/// 使 agg_minute 自愈到与 events 一致。
#[test]
fn under_agg_self_heals_on_reopen() {
    let path = tmp_db_path();
    // 红线：「今日」只在开测试前取一次作锚（同一次断言串贯穿全程），
    // fixture 时刻由该锚派生（本地正午）；后续不再各自读时钟——本地
    // 23:59 窗口重跑时「今日」串不会中途换日。
    let today = queries::today_local_str();
    let date = NaiveDate::parse_from_str(&today, "%Y-%m-%d").expect("today 必须为本地日期");
    let now_ts = noon_anchor_utc(&date).to_rfc3339();
    {
        let db = Database::open(path.to_str().unwrap()).unwrap();
        assert!(
            db.wait_for_backfill(std::time::Duration::from_secs(30)),
            "空库首开不应有回填任务"
        );
        // 模拟中断：只插 events（press 原始贡献行），不跑聚合维护
        db.insert_events(&[raw(&now_ts, "keyboard", "press", None)]);
        // db 在此 drop（等效进程退出）
    }
    let db = Database::open(path.to_str().unwrap()).unwrap();
    assert!(
        db.wait_for_backfill(std::time::Duration::from_secs(30)),
        "欠聚合自愈应在后台完成"
    );
    let conn = db.reader();
    assert!(
        agg::has_minute_for_date(&conn, &today),
        "重开后该日必须有聚合行（自愈生效）"
    );
    assert_eq!(
        queries::day_totals(&conn, &today).unwrap().keys,
        1,
        "自愈后聚合计数必须与 events 一致"
    );
    drop(conn);
    cleanup(&path);
}
