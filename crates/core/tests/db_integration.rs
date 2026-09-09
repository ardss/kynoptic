//! Database 集成测试
//!
//! 每个测试使用独立的临时数据库文件，互不污染。
//! 测试结束清理临时文件。

use kynoptic_core::db::Database;
use kynoptic_core::types::{Event, EventAction, EventType};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static SEQ: AtomicU64 = AtomicU64::new(0);

/// 生成唯一的临时数据库路径：tests_tmp/<pid>_<seq>.db
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
        "dp_test_{}_{}_{}.db",
        std::process::id(),
        seq,
        nanos
    ));
    p
}

/// 打开一个临时数据库，返回 (db, path)
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

fn kb_press() -> Event {
    Event::new(EventAction::Press, EventType::Keyboard)
}

fn mouse_click() -> Event {
    Event::new(EventAction::Click, EventType::Mouse)
}

// === 基础 CRUD ===

#[test]
fn insert_and_count_events() {
    let (db, path) = fresh_db();
    let events: Vec<Event> = (0..5).map(|_| kb_press()).collect();
    db.insert_events(&events);
    let conn = db.reader();
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM events WHERE event_type='keyboard'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 5);
    cleanup(&path);
}

#[test]
fn insert_empty_events_is_noop() {
    let (db, path) = fresh_db();
    db.insert_events(&[]); // 不应 panic
    let conn = db.reader();
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 0);
    cleanup(&path);
}

// === metadata ===

#[test]
fn metadata_set_get_and_overwrite() {
    let (db, path) = fresh_db();
    assert_eq!(db.get_metadata("missing_key"), None);
    db.set_metadata("k1", "v1");
    assert_eq!(db.get_metadata("k1"), Some("v1".into()));
    // 覆盖
    db.set_metadata("k1", "v2");
    assert_eq!(db.get_metadata("k1"), Some("v2".into()));
    cleanup(&path);
}

#[test]
fn session_lifecycle() {
    let (db, path) = fresh_db();
    let sid = db.start_session();
    assert!(sid > 0);
    db.end_session(sid, 42, 120.0);
    let conn = db.reader();
    let (total, idle): (i64, f64) = conn
        .query_row(
            "SELECT total_events, idle_seconds FROM sessions WHERE id=?1",
            rusqlite::params![sid],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(total, 42);
    assert!((idle - 120.0).abs() < 1e-6);
    cleanup(&path);
}

// === evolution ===

#[test]
fn reader_pool_multiple_concurrent_reads() {
    let (db, path) = fresh_db();
    db.insert_events(&[kb_press(), kb_press(), mouse_click()]);

    // 连续借用多个读连接（池大小 8，不会耗尽）
    let c1 = db.reader();
    let c2 = db.reader();
    let n1: i64 = c1
        .query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))
        .unwrap();
    let n2: i64 = c2
        .query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n1, 3);
    assert_eq!(n2, 3);
    // 连接在 Drop 时自动归还
    drop(c1);
    drop(c2);
    cleanup(&path);
}
