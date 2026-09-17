//! 黄金值聚合测试（审查清单 A1）：内存/临时库里插**精确已知**的事件集，
//! 断言 daily_agg / agg_minute 的**具体数值**相等——不是"非负"、不是"类型正确"。
//!
//! 直击回归："input_agg 计数被虚高 20 倍"一类 bug。任何把 COUNT(*) 行数当
//! 计数、把 samples 重复累加、或增量与重建分叉的实现都会在这里被精确断言抓住。
//!
//! 时区无关性：事件锚定在"运行时刻"附近的本地分钟（60s 间隔保证落在互不
//! 相同的本地分钟桶；同一运行内 offset 恒定，不同 UTC 分钟必映射到不同本地
//! 分钟字符串）。断言对全部桶求和的黄金总量 + 桶行数，不断言具体桶坐标。

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
        "{}\\kyn_golden_{}_{}_{}.db",
        std::env::temp_dir().display(),
        std::process::id(),
        seq,
        nanos
    )
}

fn cleanup(path: &str) {
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(format!("{path}-wal"));
    let _ = std::fs::remove_file(format!("{path}-shm"));
}

/// 锚点：当前 UTC 时刻取整到分钟，再偏移 i 分钟（互不重叠的本地分钟桶）。
fn ts_at_minute_offset(i: i64) -> String {
    let base = chrono::Utc::now()
        .with_second(0)
        .unwrap()
        .with_nanosecond(0)
        .unwrap();
    let t = base + chrono::Duration::minutes(i);
    // RFC3339 兼容格式（与采集器写入格式同族）
    t.format("%Y-%m-%dT%H:%M:%S+00:00").to_string()
}

use chrono::Timelike;

fn agg_minute_sum(db: &Database, bucket: &str, col: &str) -> i64 {
    let conn = db.reader();
    conn.query_row(
        &format!(
            "SELECT CAST(COALESCE(SUM({col}),0) AS INTEGER) FROM agg_minute WHERE bucket_id=?1"
        ),
        rusqlite::params![bucket],
        |r| r.get(0),
    )
    .unwrap()
}

fn agg_minute_rows(db: &Database, bucket: &str) -> i64 {
    let conn = db.reader();
    conn.query_row(
        "SELECT COUNT(*) FROM agg_minute WHERE bucket_id=?1",
        rusqlite::params![bucket],
        |r| r.get(0),
    )
    .unwrap()
}

fn daily_row(db: &Database) -> (i64, i64, i64) {
    let conn = db.reader();
    conn.query_row(
        "SELECT CAST(COALESCE(SUM(keys),0) AS INTEGER),
                CAST(COALESCE(SUM(clicks),0) AS INTEGER),
                CAST(COALESCE(SUM(active_minutes),0) AS INTEGER)
         FROM daily_agg",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )
    .unwrap()
}

fn recompute_daily(db: &Database) {
    db.with_writer(
        |conn| {
            kynoptic_core::daily_agg::recompute_all(conn).expect("daily_agg 重算失败");
        },
        || panic!("写连接不可用"),
    );
}

// ═══ 黄金值 1：raw press/click 形态（10 分钟 × 每分钟 5 keys + 1 click） ═══

#[test]
fn golden_raw_press_click_exact_totals() {
    let path = tmp_db_path();
    let db = Database::open(&path).expect("打开临时库失败");

    // 10 个互不相同的本地分钟：每分钟 5 次 press + 1 次 click
    // 黄金值：keys=50, clicks=10, 活跃分钟=10
    let mut events = Vec::new();
    for m in 0..10i64 {
        let ts = ts_at_minute_offset(m);
        for _ in 0..5 {
            let mut e = Event::new(EventAction::Press, EventType::Keyboard);
            e.timestamp = ts.clone();
            events.push(e);
        }
        let mut c = Event::new(EventAction::Click, EventType::Mouse);
        c.timestamp = ts.clone();
        events.push(c);
    }
    let rowids = db.insert_events(&events);
    assert_eq!(rowids.len(), 60);
    db.update_agg(&events, &rowids);

    // ── 增量维护路径的黄金值 ──
    assert_eq!(
        agg_minute_sum(&db, "input_keys", "sum_value"),
        50,
        "keys 总量黄金值 50（增量）"
    );
    assert_eq!(
        agg_minute_sum(&db, "input_keys", "count_value"),
        50,
        "keys 样本黄金值 50（增量）"
    );
    assert_eq!(
        agg_minute_sum(&db, "input_clicks", "sum_value"),
        10,
        "clicks 总量黄金值 10（增量）"
    );
    assert_eq!(
        agg_minute_rows(&db, "input_keys"),
        10,
        "10 个分钟桶（增量）"
    );

    // ── 全量重建路径必须给出完全相同的黄金值 ──
    assert!(db.rebuild_agg() > 0);
    assert_eq!(
        agg_minute_sum(&db, "input_keys", "sum_value"),
        50,
        "keys 黄金值 50（重建）"
    );
    assert_eq!(
        agg_minute_sum(&db, "input_keys", "count_value"),
        50,
        "keys 样本 50（重建）"
    );
    assert_eq!(
        agg_minute_sum(&db, "input_clicks", "sum_value"),
        10,
        "clicks 黄金值 10（重建）"
    );
    assert_eq!(
        agg_minute_rows(&db, "input_keys"),
        10,
        "10 个分钟桶（重建）"
    );

    // ── daily_agg 黄金值 ──
    recompute_daily(&db);
    let (keys, clicks, active) = daily_row(&db);
    assert_eq!(keys, 50, "daily keys 黄金值 50");
    assert_eq!(clicks, 10, "daily clicks 黄金值 10");
    assert_eq!(
        active, 10,
        "daily active_minutes 黄金值 10（10 个不同分钟）"
    );

    cleanup(&path);
}

// ═══ 黄金值 2：input_agg 计数形态（10 分钟 × 每分钟 keys=5）═══
// 直击"虚高 20 倍"：$.keys=5 的一行必须贡献 5，而不是 1（当行数数）、
// 不是 5×N（重复累加）、更不是 samples 与 keys 双算。

#[test]
fn golden_input_agg_keys_exact_not_inflated() {
    let path = tmp_db_path();
    let db = Database::open(&path).expect("打开临时库失败");

    let mut events = Vec::new();
    for m in 0..10i64 {
        let mut e = Event::new(EventAction::InputAgg, EventType::Keyboard)
            .data(serde_json::json!({"keys": 5, "keys_samples": 5, "samples": 5}));
        e.timestamp = ts_at_minute_offset(m);
        events.push(e);
    }
    let rowids = db.insert_events(&events);
    db.update_agg(&events, &rowids);

    // 增量：总量恰好 50，不是 10（按行数）、不是 100（keys+samples 双算）
    assert_eq!(
        agg_minute_sum(&db, "input_keys", "sum_value"),
        50,
        "input_agg keys 黄金值 50 = 10 分钟 × 5；虚高即回归"
    );
    assert_eq!(
        agg_minute_sum(&db, "input_keys", "count_value"),
        50,
        "样本黄金值 50"
    );
    assert_eq!(agg_minute_rows(&db, "input_keys"), 10, "10 个分钟桶");

    // 重建等价
    db.rebuild_agg();
    assert_eq!(
        agg_minute_sum(&db, "input_keys", "sum_value"),
        50,
        "重建后黄金值仍 50"
    );
    assert_eq!(agg_minute_sum(&db, "input_keys", "count_value"), 50);

    // daily：keys=50、活跃分钟=10（绝不能是 10 行 × 60 秒之类）
    recompute_daily(&db);
    let (keys, clicks, active) = daily_row(&db);
    assert_eq!(keys, 50, "daily keys 黄金值 50（input_agg 形态）");
    assert_eq!(clicks, 0, "无点击事件 → clicks 必须恰为 0");
    assert_eq!(active, 10, "active_minutes 黄金值 10");

    cleanup(&path);
}

// ═══ 黄金值 3：mouse input_agg 的 moves/move_distance_px 精确值 ═══

#[test]
fn golden_mouse_input_agg_moves_exact() {
    let path = tmp_db_path();
    let db = Database::open(&path).expect("打开临时库失败");

    let mut e = Event::new(EventAction::InputAgg, EventType::Mouse).data(serde_json::json!({
        "clicks": 3, "samples": 20, "moves": 10, "move_distance_px": 500,
    }));
    e.timestamp = ts_at_minute_offset(0);
    let events = vec![e];
    let rowids = db.insert_events(&events);
    db.update_agg(&events, &rowids);

    let conn = db.reader();
    let (click_sum, click_cnt): (i64, i64) = conn
        .query_row(
            "SELECT CAST(COALESCE(sum_value,0) AS INTEGER), CAST(COALESCE(count_value,0) AS INTEGER)
             FROM agg_minute WHERE bucket_id='input_clicks'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        (click_sum, click_cnt),
        (3, 20),
        "clicks sum=$.clicks=3, count=samples=20"
    );

    let (mv_sum, mv_cnt): (i64, i64) = conn
        .query_row(
            "SELECT CAST(COALESCE(sum_value,0) AS INTEGER), CAST(COALESCE(count_value,0) AS INTEGER)
             FROM agg_minute WHERE bucket_id='input_moves'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(mv_sum, 500, "input_moves sum=$.move_distance_px=500");
    assert_eq!(
        mv_cnt, 10,
        "input_moves count=$.moves=10（绝不能取 $.samples=20）"
    );

    // 重建逐值等价
    db.rebuild_agg();
    let (mv_sum2, mv_cnt2): (i64, i64) = conn
        .query_row(
            "SELECT CAST(COALESCE(sum_value,0) AS INTEGER), CAST(COALESCE(count_value,0) AS INTEGER)
             FROM agg_minute WHERE bucket_id='input_moves'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        (mv_sum, mv_cnt),
        (mv_sum2, mv_cnt2),
        "重建后 input_moves 黄金值不变"
    );

    cleanup(&path);
}

// ═══ 黄金值 4：window switch → agg_daily app 行精确计数 ═══

#[test]
fn golden_daily_app_counts_exact() {
    let path = tmp_db_path();
    let db = Database::open(&path).expect("打开临时库失败");

    let mut events = Vec::new();
    // code.exe ×3、web.exe ×2（不同分钟，避免 input_agg UPSERT 干扰——switch 不走 UPSERT）
    for (i, (app, n)) in [("code.exe", 3i64), ("web.exe", 2i64)].iter().enumerate() {
        for j in 0..*n {
            let mut e = Event::new(EventAction::Switch, EventType::Window).app(*app, "title");
            e.timestamp = ts_at_minute_offset((i as i64) * 10 + j);
            events.push(e);
        }
    }
    let rowids = db.insert_events(&events);
    db.update_agg(&events, &rowids);

    let conn = db.reader();
    let app_cnt = |name: &str| -> i64 {
        conn.query_row(
            "SELECT CAST(COALESCE(SUM(count_value),0) AS INTEGER) FROM agg_daily WHERE bucket_id=?1",
            rusqlite::params![format!("app:{name}")],
            |r| r.get(0),
        )
        .unwrap()
    };
    assert_eq!(app_cnt("code.exe"), 3, "agg_daily app:code.exe 黄金值 3");
    assert_eq!(app_cnt("web.exe"), 2, "agg_daily app:web.exe 黄金值 2");

    db.rebuild_agg();
    assert_eq!(app_cnt("code.exe"), 3, "重建后 app 计数黄金值不变");
    assert_eq!(app_cnt("web.exe"), 2);

    cleanup(&path);
}
