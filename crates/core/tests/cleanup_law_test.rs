//! 清理铁律测试（审查清单 A2）——四表版。
//!
//! 铁律：retention = 0（默认）时，`cleanup_old_events` / `maintenance()` 后
//! **events、agg_minute、agg_daily、sessions 四张表行数全部不变**。
//! 旧测试只数 events；聚合缓存与 session 历史同样是用户数据，
//! 被维护路径误清同样违反"原始/派生数据只增不删"契约。

use kynoptic_core::db::Database;
use kynoptic_core::types::{Event, EventAction, EventType};
use std::sync::atomic::{AtomicU64, Ordering};

static SEQ: AtomicU64 = AtomicU64::new(0);

fn cleanup_files(path: &str) {
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(format!("{path}-wal"));
    let _ = std::fs::remove_file(format!("{path}-shm"));
}

/// 四表行数快照（铁律的度量对象）
fn four_table_counts(db: &Database) -> (i64, i64, i64, i64) {
    let conn = db.reader();
    let c = |sql: &str| -> i64 { conn.query_row(sql, [], |r| r.get(0)).unwrap() };
    (
        c("SELECT COUNT(*) FROM events"),
        c("SELECT COUNT(*) FROM agg_minute"),
        c("SELECT COUNT(*) FROM agg_daily"),
        c("SELECT COUNT(*) FROM sessions"),
    )
}

/// 造一个有真实数据的库：raw 事件 + input_agg + 窗口切换（agg_daily app 行）+ session
fn seeded_db(tag: &str) -> (Database, String) {
    let path = format!(
        "{}\\kyn_cleanup_seed_{tag}_{}_{}.db",
        std::env::temp_dir().display(),
        std::process::id(),
        SEQ.fetch_add(1, Ordering::SeqCst)
    );
    let db = Database::open(&path).expect("打开临时库失败");
    let base = chrono::Utc::now() - chrono::Duration::hours(2);
    let ts = |mins: i64| -> String {
        (base + chrono::Duration::minutes(mins))
            .format("%Y-%m-%dT%H:%M:%S+00:00")
            .to_string()
    };
    let mut events = Vec::new();
    for m in 0..5i64 {
        let mut e = Event::new(EventAction::Press, EventType::Keyboard);
        e.timestamp = ts(m);
        events.push(e);
    }
    let mut c = Event::new(EventAction::Click, EventType::Mouse);
    c.timestamp = ts(1);
    events.push(c);
    let mut w = Event::new(EventAction::Switch, EventType::Window).app("code.exe", "t");
    w.timestamp = ts(2);
    events.push(w);
    let mut ia = Event::new(EventAction::InputAgg, EventType::Keyboard)
        .data(serde_json::json!({"keys": 7, "samples": 7}));
    ia.timestamp = ts(3);
    events.push(ia);

    let rowids = db.insert_events(&events);
    db.update_agg(&events, &rowids);
    let sid = db.start_session();
    assert!(sid > 0);
    (db, path)
}

#[test]
fn cleanup_zero_leaves_all_four_tables_untouched() {
    let (db, path) = seeded_db("zero");
    let before = four_table_counts(&db);
    assert!(before.0 >= 7, "前置：events 已有数据 ({before:?})");
    assert!(before.1 >= 2, "前置：agg_minute 已有数据 ({before:?})");
    assert!(before.2 >= 1, "前置：agg_daily 已有数据 ({before:?})");
    assert!(before.3 >= 1, "前置：sessions 已有数据 ({before:?})");

    // 默认 retention = 0：cleanup 必须是 no-op
    let deleted = db.cleanup_old_events();
    assert_eq!(deleted, 0, "retention=0 时 cleanup 不得删除任何行");
    let after = four_table_counts(&db);
    assert_eq!(before, after, "cleanup 0 后四表行数必须全部不变");
    cleanup_files(&path);
}

#[test]
fn full_maintenance_leaves_all_four_tables_untouched() {
    let (db, path) = seeded_db("maint");
    let before = four_table_counts(&db);

    // maintenance = cleanup + 刷 daily_agg + checkpoint + VACUUM：同样不得动四表
    db.maintenance();
    let after = four_table_counts(&db);
    assert_eq!(before, after, "完整 maintenance 后四表行数必须全部不变");
    cleanup_files(&path);
}

#[test]
fn default_retention_is_zero_by_contract() {
    // 契约锚：默认保留天数必须是 0（永不删除）。若有人改默认值，
    // 本测试强迫其在审查中显式确认（用户数据不可静默可清）。
    assert_eq!(
        kynoptic_core::constants::DEFAULT_RETENTION_DAYS,
        0,
        "DEFAULT_RETENTION_DAYS 必须为 0（铁律：清理必须显式 opt-in）"
    );
}
