//! perf-write — 逐事件存储成本基准（DISK FOOTPRINT bench 的重度部分）
//!
//! 通过 core 的真实写路径（`Database::insert_events`，与 writer 线程相同的
//! 300 条/批 + prepare_cached）插入 100,000 条真实形态的事件
//! （40% mouse/move、20% keyboard/press、20% window/switch、20% system/heartbeat），
//! 报告吞吐与 DB 每事件字节数（WAL checkpoint 后）。
//!
//! 运行：`cargo run --release -p kynoptic-core --example perf-write`
//! 输出 JSON：吞吐 (events/s)、db_bytes、bytes_per_event。

use std::time::Instant;

use kynoptic_core::db::Database;
use kynoptic_core::types::{Event, EventAction, EventType};

const TOTAL: usize = 100_000;
const BATCH: usize = 300; // 与 constants::WRITE_BATCH_SIZE 一致

fn make_event(i: usize, base_ts: chrono::DateTime<chrono::Utc>) -> Event {
    let ts = base_ts + chrono::Duration::milliseconds(i as i64);
    let mut ev = match i % 5 {
        0 | 1 => Event::new(EventAction::Move, EventType::Mouse) // 40% mouse moves
            .data(serde_json::json!({ "x": 100 + i % 800, "y": 200 + i % 600 }))
            .app("", ""),
        2 => Event::new(EventAction::Press, EventType::Keyboard)
            .data(serde_json::json!({ "key": "a", "modifiers": [] }))
            .app("code.exe", "main.rs - editor"),
        3 => Event::new(EventAction::Switch, EventType::Window)
            .data(serde_json::json!({ "hwnd": 1000 + i as u64, "title": "Document", "pid": 4242 }))
            .app("word.exe", "Document - Word"),
        _ => Event::new(EventAction::Heartbeat, EventType::System).data(serde_json::json!({
            "memory": { "total": 34359738368u64, "available": 17179869184u64, "used_percent": 50 },
            "cpu_percent": 12.5,
        })),
    };
    ev.timestamp = ts.to_rfc3339();
    ev
}

fn db_bytes(db_path: &str) -> u64 {
    let main = std::fs::metadata(db_path).map(|m| m.len()).unwrap_or(0);
    let wal = std::fs::metadata(format!("{db_path}-wal"))
        .map(|m| m.len())
        .unwrap_or(0);
    main + wal
}

fn main() {
    let db_path = std::env::var("KYNOPTIC_DB").unwrap_or_else(|_| "target/perf-write.db".into());
    let _ = std::fs::remove_file(&db_path);

    let db = Database::open(&db_path).expect("open db");
    let base_ts = chrono::Utc::now();

    // 构造事件（不计入写入耗时）
    let events: Vec<Event> = (0..TOTAL).map(|i| make_event(i, base_ts)).collect();

    // 与 writer 线程一致：300 条/批走 insert_events
    let t0 = Instant::now();
    for chunk in events.chunks(BATCH) {
        db.insert_events(chunk);
    }
    let elapsed = t0.elapsed().as_secs_f64();

    // 压缩到主库文件后测字节数（模拟 maintenance 的 checkpoint）
    let conn_size_before = db_bytes(&db_path);
    {
        // 用一次 wal_checkpoint(TRUNCATE) 合并 WAL（同 maintenance 行为）
        db.maintenance();
    }
    let bytes = db_bytes(&db_path);

    let events_count: i64 = {
        let r = db.reader();
        r.query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
            .unwrap_or(0)
    };

    println!(
        r#"{{"bench":"perf-write","events":{events_count},"elapsed_secs":{elapsed:.3},
  "throughput_events_per_sec":{:.0},
  "db_bytes_before_checkpoint":{conn_size_before},"db_bytes_after_maintenance":{bytes},
  "bytes_per_event":{:.1}}}"#,
        TOTAL as f64 / elapsed,
        bytes as f64 / events_count.max(1) as f64,
    );

    // ── 信息对照（非默认）：minute 粒度（opt-in input_granularity="minute"）──
    // 同一合成工作负载下，把 keyboard/mouse 折叠为每分钟每桶一行 input_agg
    // 计数行（window/system 事件原样保留），测量存储形态差异。raw 仍是默认。
    let minute_db = format!("{db_path}.minute");
    let _ = std::fs::remove_file(&minute_db);
    let dbm = Database::open(&minute_db).expect("open minute db");
    use std::collections::BTreeMap;
    let mut buckets: BTreeMap<String, (u64, u64, u64, u64)> = BTreeMap::new(); // minute -> (keys,clicks,moves,samples)
    let mut minute_events: Vec<Event> = Vec::new();
    for e in &events {
        match (e.event_type, e.event_action) {
            (EventType::Keyboard, EventAction::Press) => {
                let m = &e.timestamp[..16];
                buckets.entry(m.to_string()).or_default().0 += 1;
                buckets.get_mut(m).unwrap().3 += 1;
            }
            (EventType::Mouse, EventAction::Move) => {
                let m = &e.timestamp[..16];
                buckets.entry(m.to_string()).or_default().2 += 1;
                buckets.get_mut(m).unwrap().3 += 1;
            }
            _ => minute_events.push(e.clone()),
        }
    }
    for (minute, (keys, clicks, moves, samples)) in buckets {
        let ts = format!("{minute}:00+00:00");
        if keys > 0 {
            let mut e = Event::new(EventAction::InputAgg, EventType::Keyboard)
                .data(serde_json::json!({ "keys": keys, "samples": samples }));
            e.timestamp = ts.clone();
            minute_events.push(e);
        }
        if clicks + moves > 0 {
            let mut e =
                Event::new(EventAction::InputAgg, EventType::Mouse).data(serde_json::json!({
                    "clicks": clicks, "scroll_ticks": 0, "moves": moves,
                    "move_distance_px": 0, "samples": samples,
                }));
            e.timestamp = ts;
            minute_events.push(e);
        }
    }
    minute_events.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));
    let t1 = Instant::now();
    for chunk in minute_events.chunks(BATCH) {
        dbm.insert_events(chunk);
    }
    let minute_elapsed = t1.elapsed().as_secs_f64();
    dbm.maintenance();
    let minute_bytes = db_bytes(&minute_db);
    let minute_rows: i64 = {
        let r = dbm.reader();
        r.query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
            .unwrap_or(0)
    };
    println!(
        r#"{{"bench":"perf-write-minute","opt_in":true,"source_events":{TOTAL},
  "stored_rows":{minute_rows},"elapsed_secs":{minute_elapsed:.3},
  "db_bytes_after_maintenance":{minute_bytes},
  "bytes_vs_raw_default":"{:.1}x smaller","rows_vs_raw_default":"{:.1}x fewer"}}"#,
        bytes as f64 / minute_bytes.max(1) as f64,
        events_count as f64 / minute_rows.max(1) as f64,
    );
}
