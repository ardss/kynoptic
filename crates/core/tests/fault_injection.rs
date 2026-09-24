//! 故障注入测试（并发可靠性）。
//!
//! 覆盖场景：
//! 1. writer 满载/写阻塞 —— 受控慢消费者（独占写连接）下的采集端韧性；
//! 2. 停机风暴 —— 10 轮快速 start/shutdown（含 hook stop 释放 sender 路径）；
//! 3. 消费者死亡 —— Disconnected 语义 + 同库重启恢复；
//! 5. agg 重算与写并发 —— backfill_all 与持续 insert 并发的一致性；
//! 7. 跨午夜滚动 —— 23:59→00:00 的 drain/flush 序列与分钟/日期归属。
//!
//! 注入限制（缺 API，详见报告）：send_event / DROPPED_EVENTS / 内部 channel
//! 均未公开，无法直接注入"channel 写满 20000 后继续 send"并读取丢弃计数；
//! 场景 1 以独占写连接模拟 writer 停滞（等效的通道堆积方向），丢弃计数断言
//! 待补 API。

use std::collections::HashSet;
use std::sync::Mutex;
use std::time::Duration;

use chrono::{Datelike, Local, TimeZone, Timelike};
use rusqlite::Connection;

use kynoptic_core::collector::{start_collection_custom, CollectorSettings, InputGranularity};
use kynoptic_core::db::Database;
use kynoptic_core::types::{Event, EventAction, EventType};

/// input_agg 全局原子与 collector 全局态被同进程全部测试共享，串行化。
static SEQ: Mutex<()> = Mutex::new(());

fn seq_guard() -> std::sync::MutexGuard<'static, ()> {
    SEQ.lock().unwrap_or_else(|e| e.into_inner())
}

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "kyn-fi-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn hook_only_enabled() -> HashSet<String> {
    ["keyboard_hook", "mouse_hook"]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

fn open_sessions(conn: &Connection) -> i64 {
    conn.query_row(
        "SELECT COUNT(*) FROM sessions WHERE end_time IS NULL",
        [],
        |r| r.get(0),
    )
    .unwrap()
}

// ─── 场景 1：writer 停滞（满载注入的可行子集，缺 DROPPED_EVENTS 读取 API） ─────

/// 注入：独占写连接 5 秒 → writer 的 insert_events 阻塞、事件在 channel 堆积。
/// 断言：采集端（monitor/hook/聚合线程）不 panic；写锁释放后 writer 恢复落库；
/// shutdown 正常收敛、session 关闭。
///
/// 无法断言的部分：channel 真实写满 20000 后 send_event 的 DROPPED_EVENTS 增长
/// （send_event 为 pub(crate)、计数器为私有 static、channel 容量写死 20000）。
#[test]
fn fi1_writer_stall_recovers_without_panic() {
    let _g = seq_guard();
    let dir = temp_dir("writer-stall");
    let db_path = dir.join("kyn.db").to_string_lossy().to_string();
    let enabled: HashSet<String> = kynoptic_core::registry::default_enabled_ids()
        .into_iter()
        .map(String::from)
        .collect();
    let settings = CollectorSettings {
        input_granularity: InputGranularity::Raw,
        write_flush_interval_secs: 1,
        vk_frequency_enabled: true,
        redact_titles: false,
    };
    let mut col = start_collection_custom(&enabled, settings, &db_path);

    // 注入：后台线程独占写连接 5 秒，writer 在 lock_writer 上排队
    let db = col.db.clone();
    let holder = std::thread::spawn(move || {
        db.with_writer(|_| std::thread::sleep(Duration::from_secs(5)), || ());
    });
    std::thread::sleep(Duration::from_millis(2500)); // 期间事件持续入 channel
    holder.join().unwrap();

    // 释放后 writer 必须恢复落库
    std::thread::sleep(Duration::from_secs(3));
    let written = col.total_written.load(std::sync::atomic::Ordering::Relaxed);
    assert!(
        written > 0,
        "写锁释放后 writer 必须恢复落库（total_written={written}）"
    );

    col.shutdown();
    let conn = Connection::open(&db_path).unwrap();
    assert_eq!(open_sessions(&conn), 0, "停机后不得残留 open session");
    let _ = conn.close();
    let _ = std::fs::remove_dir_all(&dir);
}

// ─── 场景 2：停机风暴 ────────────────────────────────────────────────────────

/// 10 轮快速 start/shutdown（Minute/Raw 交替，全部启用真实 hook 以覆盖
/// hook stop 释放 sender 的新代码路径）。整体跑在子线程并带 180s 超时看门狗，
/// 任何一轮卡死（死锁）都会以超时失败暴露。
#[test]
fn fi2_shutdown_storm_ten_rounds_no_deadlock_no_ghosts() {
    let _g = seq_guard();
    let dir = temp_dir("shutdown-storm");
    let fi_dir = dir.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let run = || -> std::result::Result<Vec<i64>, String> {
            let dir = fi_dir;
            let mut ids = Vec::new();
            for round in 0..10 {
                let settings = CollectorSettings {
                    input_granularity: if round % 2 == 0 {
                        InputGranularity::Minute
                    } else {
                        InputGranularity::Raw
                    },
                    write_flush_interval_secs: 1,
                    vk_frequency_enabled: true,
                    redact_titles: false,
                };
                let db_path = dir
                    .join(format!("r{round}.db"))
                    .to_string_lossy()
                    .to_string();
                let mut col = start_collection_custom(&hook_only_enabled(), settings, &db_path);
                ids.push(col.session_id);
                col.shutdown();

                let conn = Connection::open(&db_path).map_err(|e| e.to_string())?;
                let open_cnt = open_sessions(&conn);
                if open_cnt != 0 {
                    return Err(format!("round {round}: {open_cnt} 个未关闭 session"));
                }
                let end: Option<String> = conn
                    .query_row(
                        "SELECT end_time FROM sessions WHERE id = ?1",
                        [ids[ids.len() - 1]],
                        |r| r.get(0),
                    )
                    .map_err(|e| e.to_string())?;
                if end.is_none() {
                    return Err(format!("round {}: session 未正常关闭", ids[ids.len() - 1]));
                }
                let _ = conn.close();
            }
            Ok(ids)
        };
        let _ = tx.send(run());
    });
    let res = rx
        .recv_timeout(Duration::from_secs(180))
        .expect("停机风暴 180s 未完成：疑似死锁");
    let ids = res.expect("风暴轮次失败");
    assert_eq!(ids.len(), 10);
    // 每轮独立 db 文件，session id 各自从 1 起：只断言每轮都有有效新 session
    // （关闭与无幽灵已在轮内逐轮断言）。
    assert!(ids.iter().all(|id| *id > 0), "每轮必须新建 session");
    let _ = std::fs::remove_dir_all(&dir);
}

// ─── 场景 3：消费者死亡（Disconnected 语义 + 重启恢复） ──────────────────────

/// send_event 的 Disconnected 分支输入前提：接收端 drop 后 try_send 返回
/// Disconnected 且不 panic。端到端"writer 死亡后继续 send"注入因 send_event /
/// 内部 channel 不公开而受限，恢复语义以同库 stop→start 验证。
#[test]
fn fi3_consumer_death_semantics_and_restart_recovery() {
    let _g = seq_guard();
    // 前提语义：Disconnected 是静默可判定的
    let (tx, rx) = crossbeam_channel::bounded::<Event>(4);
    drop(rx);
    let res = tx.try_send(Event::new(EventAction::Press, EventType::Keyboard));
    assert!(
        matches!(res, Err(crossbeam_channel::TrySendError::Disconnected(_))),
        "接收端 drop 后必须返回 Disconnected"
    );

    // 端到端：同库 stop → start 恢复，session 连续、无幽灵
    let dir = temp_dir("consumer-death");
    let db_path = dir.join("kyn.db").to_string_lossy().to_string();
    let settings = CollectorSettings {
        input_granularity: InputGranularity::Raw,
        write_flush_interval_secs: 1,
        vk_frequency_enabled: true,
        redact_titles: false,
    };
    let enabled = hook_only_enabled();
    let mut c1 = start_collection_custom(&enabled, settings, &db_path);
    let s1 = c1.session_id;
    c1.shutdown();

    let mut c2 = start_collection_custom(&enabled, settings, &db_path);
    let s2 = c2.session_id;
    assert!(s2 > s1, "重启必须新建 session");
    c2.shutdown();

    let conn = Connection::open(&db_path).unwrap();
    assert_eq!(open_sessions(&conn), 0, "重启两轮后不得残留 open session");
    let _ = conn.close();
    let _ = std::fs::remove_dir_all(&dir);
}

// ─── 场景 5：agg 重算与写并发 ────────────────────────────────────────────────

/// 连接 A（Database writer）持续 insert + 增量 agg；连接 B（裸 Connection）
/// 反复跑 backfill_all 式重算事务。断言：重算事务不因 busy 失败、events 无丢行、
/// 增量 agg 与 events 真值一致（并发分块 DELETE/INSERT 未造成丢失或重复）、
/// rebuild 后重放最后一批增量被 max_event_rowid 守卫跳过。
#[test]
fn fi5_agg_rebuild_concurrent_with_writes_consistent() {
    let dir = temp_dir("agg-conc");
    let path = dir.join("kyn.db");
    let db = std::sync::Arc::new(Database::open(path.to_str().unwrap()).unwrap());
    assert!(
        db.wait_for_backfill(Duration::from_secs(5)),
        "空库回填应立即完成"
    );

    let bf_conn = Connection::open(&path).unwrap();
    kynoptic_core::db::apply_pragmas(&bf_conn).unwrap();

    // 起跑门：writer 落地第一批事件后再启动重算线程，保证 3 趟重算与后续
    // 写入真正交错（否则空库趟会把 cursor 置 done 且返回 0 行，属空转）。
    let (gate_tx, gate_rx) = std::sync::mpsc::channel::<()>();

    let db_w = db.clone();
    let writer = std::thread::spawn(
        move || -> std::result::Result<(i64, Vec<i64>, Vec<Event>), String> {
            let mut total = 0i64;
            let mut last_rowids = Vec::new();
            let mut last_batch = Vec::new();
            let base = chrono::Utc::now() - chrono::Duration::hours(2);
            for i in 0..100i64 {
                let mut batch = Vec::with_capacity(20);
                for j in 0..20i64 {
                    let mut e = Event::new(EventAction::Press, EventType::Keyboard);
                    e.timestamp =
                        (base + chrono::Duration::minutes(i) + chrono::Duration::seconds(j % 50))
                            .to_rfc3339();
                    batch.push(e);
                }
                let rowids = db_w.insert_events(&batch);
                db_w.update_agg(&batch, &rowids);
                total += 20;
                last_batch = batch;
                last_rowids = rowids;
                if i == 0 {
                    let _ = gate_tx.send(());
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            Ok((total, last_rowids, last_batch))
        },
    );

    gate_rx
        .recv_timeout(Duration::from_secs(30))
        .expect("writer 首批落库超时");
    let rebuilder = std::thread::spawn(move || -> std::result::Result<usize, String> {
        let mut last = 0usize;
        for pass in 0..3 {
            last = kynoptic_core::db::agg::backfill_all(&bf_conn)
                .map_err(|e| format!("backfill pass {pass} 失败（busy/其他）: {e}"))?;
        }
        Ok(last)
    });

    let (total, last_rowids, last_batch) = writer.join().unwrap().expect("writer 线程失败");
    let backfilled = rebuilder.join().unwrap().expect("rebuild 线程失败");

    let conn = Connection::open(&path).unwrap();
    kynoptic_core::db::apply_pragmas(&conn).unwrap();
    assert!(backfilled > 0, "重算必须产出 agg 行");

    // events 无丢行（insert_events 的 busy/降级路径不得吞行）
    let events_cnt: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM events WHERE event_type='keyboard' AND event_action='press'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(events_cnt, total, "并发重算期间 events 不得丢行");

    // 增量 agg 与 events 真值一致（并发分块重算未造成丢失/重复）
    let inc_keys: i64 = conn
        .query_row(
            "SELECT CAST(COALESCE(SUM(sum_value),0) AS INTEGER) FROM agg_minute WHERE bucket_id='input_keys'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        inc_keys, total,
        "增量 agg 必须与 events 一致（并发 backfill 下）"
    );

    // max_event_rowid 守卫：全量重建后重放最后一批增量，不得重复累计
    kynoptic_core::db::agg::rebuild_all(&conn).unwrap();
    db.update_agg(&last_batch, &last_rowids);
    let after_replay: i64 = conn
        .query_row(
            "SELECT CAST(COALESCE(SUM(sum_value),0) AS INTEGER) FROM agg_minute WHERE bucket_id='input_keys'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(after_replay, total, "重建后迟到增量必须被守卫跳过");
    let _ = conn.close();
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

// ─── 场景 7：跨午夜滚动（23:59 → 00:00） ─────────────────────────────────────

/// 受控分钟 key 驱动 input_agg 的 drain/flush 序列跨午夜：
/// 23:59 建桶 → 计数 → 00:00 rollover 折叠出 23:59 完整分钟（时间戳/日期归属
/// 不得漂移）→ 00:00 partial flush。rollover+抑制组合依赖 pub(crate) activate，
/// 外部测试无法组合，由 src 内单测 suppress_minute_survives_rollover 覆盖。
#[test]
fn fi7_midnight_rollover_drain_flush_sequences() {
    let _g = seq_guard();
    kynoptic_core::input_agg::reset();
    let t2359 = Local
        .with_ymd_and_hms(2026, 1, 31, 23, 59, 0)
        .earliest()
        .unwrap();
    let t0000 = Local
        .with_ymd_and_hms(2026, 2, 1, 0, 0, 0)
        .earliest()
        .unwrap();

    // 23:59：建桶（drain 不产出）
    kynoptic_core::input_agg::record_key();
    kynoptic_core::input_agg::record_key();
    assert!(
        kynoptic_core::input_agg::drain(t2359).is_empty(),
        "建桶不产出"
    );
    // 仍在 23:59：继续累计（这批在 rollover 前未被 drain 的计数，按既有语义
    // counters_preserved_across_rollover：rollover 时开启新分钟桶，归属 00:00）
    kynoptic_core::input_agg::record_key();
    kynoptic_core::input_agg::record_click_button(0, false);

    // 跨午夜：rollover 折叠出 23:59 已入桶的完整分钟（2 键）
    let evts = kynoptic_core::input_agg::drain(t0000);
    assert_eq!(evts.len(), 1, "只有 keyboard 一行: {evts:?}");
    let key_of = |evts: &[Event], i: usize, k: &str| -> u64 {
        evts[i]
            .event_data
            .as_ref()
            .and_then(|v| v.get(k))
            .and_then(|v| v.as_u64())
            .unwrap_or(0)
    };
    assert_eq!(key_of(&evts, 0, "keys"), 2);
    // 时间戳必须钉在被折叠的 23:59 分钟，日期归属跨年前一天
    for e in &evts {
        let t = chrono::DateTime::parse_from_rfc3339(&e.timestamp)
            .unwrap()
            .with_timezone(&Local);
        assert_eq!(
            (t.month(), t.day(), t.hour(), t.minute()),
            (1, 31, 23, 59),
            "rollover 事件时间戳必须归属 23:59: {}",
            e.timestamp
        );
    }

    // 00:00 新桶：partial flush 输出 rollover 时转入的未满分钟（1 键 1 点击），
    // 归属次日 00:00
    let evts = kynoptic_core::input_agg::flush_partial(t0000);
    assert_eq!(evts.len(), 2, "keyboard+mouse 各一行: {evts:?}");
    let kb = evts
        .iter()
        .position(|e| e.event_type == EventType::Keyboard)
        .unwrap();
    let ms = evts
        .iter()
        .position(|e| e.event_type == EventType::Mouse)
        .unwrap();
    assert_eq!(key_of(&evts, kb, "keys"), 1);
    assert_eq!(key_of(&evts, ms, "clicks"), 1);
    let t = chrono::DateTime::parse_from_rfc3339(&evts[0].timestamp)
        .unwrap()
        .with_timezone(&Local);
    assert_eq!(
        (t.month(), t.day(), t.hour(), t.minute()),
        (2, 1, 0, 0),
        "partial flush 必须归属次日 00:00"
    );
    // flush 后状态清零：不重复产出
    assert!(kynoptic_core::input_agg::flush_partial(t0000).is_empty());
    kynoptic_core::input_agg::reset();
}
