//! 冻结时钟跨午夜测试（审查清单 A4）：
//! 固定构造本地 23:59 与次日 00:01 的事件，断言 agg_minute / daily_agg
//! 的桶归属精确正确。不用 Local::now() 参与任何断言（仅取一次"今天"作锚），
//! 桶坐标在测试里手工算死——跨午夜错桶（23:59 记到昨天、00:01 记到今天、
//! 或两边都记进同一天）在这里直接红。

use chrono::{DateTime, Datelike, Local, TimeZone, Utc};
use kynoptic_core::db::Database;
use kynoptic_core::types::{Event, EventAction, EventType};
use std::sync::atomic::{AtomicU64, Ordering};

static SEQ: AtomicU64 = AtomicU64::new(0);

fn tmp_db_path() -> String {
    let seq = SEQ.fetch_add(1, Ordering::SeqCst);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64 + d.as_secs() * 1_000_000_000)
        .unwrap_or(0);
    format!(
        "{}\\kyn_midnight_{}_{}_{}.db",
        std::env::temp_dir().display(),
        std::process::id(),
        seq,
        nanos
    )
}

fn cleanup_files(path: &str) {
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(format!("{path}-wal"));
    let _ = std::fs::remove_file(format!("{path}-shm"));
}

/// 本地钟面时刻（Y, M, D, h, m）→ UTC RFC3339 字符串（冻结时钟：与 now 无关，
/// 只在锚定"今天"时取一次本地日期）。
fn local_ts_ymd(y: i32, m: u32, d: u32, h: u32, min: u32) -> String {
    let naive = chrono::NaiveDate::from_ymd_opt(y, m, d)
        .unwrap()
        .and_hms_opt(h, min, 30)
        .unwrap();
    let dt: DateTime<Local> = Local
        .from_local_datetime(&naive)
        .earliest()
        .expect("构造的本地时刻不存在（DST 跳变日请换锚点日期）");
    dt.with_timezone(&Utc).to_rfc3339()
}

/// 今日 / 明日的本地日期（唯一允许触碰时钟的地方——只取日期锚点）
fn anchor_dates() -> (chrono::NaiveDate, chrono::NaiveDate) {
    let today = Local::now().date_naive();
    (today, today.succ_opt().unwrap())
}

fn minute_bucket_sum(db: &Database, date: &str, hour: i64, minute: i64) -> i64 {
    let conn = db.reader();
    conn.query_row(
        "SELECT CAST(COALESCE(SUM(sum_value),0) AS INTEGER) FROM agg_minute
         WHERE date=?1 AND hour=?2 AND minute=?3 AND bucket_id='input_keys'",
        rusqlite::params![date, hour, minute],
        |r| r.get(0),
    )
    .unwrap()
}

fn daily_keys(db: &Database, date: &str) -> i64 {
    let conn = db.reader();
    conn.query_row(
        "SELECT CAST(COALESCE(keys,0) AS INTEGER) FROM daily_agg WHERE date=?1",
        rusqlite::params![date],
        |r| r.get(0),
    )
    .unwrap_or(0)
}

#[test]
fn events_at_2359_and_0001_land_in_correct_local_day_buckets() {
    let (today, tomorrow) = anchor_dates();
    let (ty, tm, td) = (today.year(), today.month(), today.day());
    let (ny, nm, nd) = (tomorrow.year(), tomorrow.month(), tomorrow.day());

    let path = tmp_db_path();
    let db = Database::open(&path).expect("打开临时库失败");

    // 23:59 ×3、次日 00:01 ×2（本地钟面固定构造）
    let mut events = Vec::new();
    for _ in 0..3 {
        let mut e = Event::new(EventAction::Press, EventType::Keyboard);
        e.timestamp = local_ts_ymd(ty, tm, td, 23, 59);
        events.push(e);
    }
    for _ in 0..2 {
        let mut e = Event::new(EventAction::Press, EventType::Keyboard);
        e.timestamp = local_ts_ymd(ny, nm, nd, 0, 1);
        events.push(e);
    }
    let rowids = db.insert_events(&events);
    db.update_agg(&events, &rowids);

    // ── agg_minute：增量维护路径的桶归属 ──
    assert_eq!(
        minute_bucket_sum(&db, &today.format("%Y-%m-%d").to_string(), 23, 59),
        3,
        "本地 23:59 的 3 次按键必须落在今日 23:59 桶"
    );
    assert_eq!(
        minute_bucket_sum(&db, &tomorrow.format("%Y-%m-%d").to_string(), 0, 1),
        2,
        "次日 00:01 的 2 次按键必须落在明日 00:01 桶"
    );

    // ── 全量重建路径同判定 ──
    db.rebuild_agg();
    assert_eq!(
        minute_bucket_sum(&db, &today.format("%Y-%m-%d").to_string(), 23, 59),
        3,
        "重建后 23:59 桶仍为 3"
    );
    assert_eq!(
        minute_bucket_sum(&db, &tomorrow.format("%Y-%m-%d").to_string(), 0, 1),
        2,
        "重建后 00:01 桶仍为 2"
    );

    // ── daily_agg：按本地日切分的黄金值 ──
    db.with_writer(
        |conn| {
            kynoptic_core::daily_agg::recompute_all(conn).expect("daily_agg 重算失败");
        },
        || panic!("写连接不可用"),
    );
    assert_eq!(
        daily_keys(&db, &today.format("%Y-%m-%d").to_string()),
        3,
        "今日 keys=3（只有 23:59 那 3 次）"
    );
    assert_eq!(
        daily_keys(&db, &tomorrow.format("%Y-%m-%d").to_string()),
        2,
        "明日 keys=2（只有 00:01 那 2 次）"
    );

    // 除这两天外，不得有别的日期行吸收了事件
    let conn = db.reader();
    let other: i64 = conn
        .query_row(
            "SELECT CAST(COALESCE(SUM(keys),0) AS INTEGER) FROM daily_agg WHERE date NOT IN (?1, ?2)",
            rusqlite::params![today.format("%Y-%m-%d").to_string(), tomorrow.format("%Y-%m-%d").to_string()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(other, 0, "跨午夜事件漏到了第三天");

    cleanup_files(&path);
}
