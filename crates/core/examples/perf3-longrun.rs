//! perf3-longrun — Long-run robustness bench (scenarios 1, 2, 4 + agg-growth cost).
//!
//! Phases:
//!   P1  agg-maintenance cost vs agg_minute size: time update_agg on a flush-sized
//!       batch with (a) empty agg_minute, (b) 525k rows (the owner's "1 year" figure).
//!   P2  sustained write via the REAL writer path (insert_events + update_agg) for
//!       KYNOPTIC_PERF3_SECS (default 1800), sampling RSS / WAL size — trend lines.
//!       Concurrent reader load runs during the whole window (scenario 4): WAL mode
//!       check + readers-never-block-writer + post-run events↔agg consistency.
//!   P3  burst + stalled-writer worst case: fill the real bounded channel (writer
//!       NOT draining = infinite disk stall), measure enqueue latency (hook path
//!       must not block), drops, queue RSS (worst-case memory), then drain.
//!
//! Run: KYNOPTIC_PERF3_SECS=1800 cargo run --release -p kynoptic-core --example perf3-longrun

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use crossbeam_channel::bounded;
use kynoptic_core::constants::{CHANNEL_CAPACITY, WRITE_BATCH_SIZE};
use kynoptic_core::db::Database;
use kynoptic_core::types::{Event, EventAction, EventType};

const FLUSH: usize = WRITE_BATCH_SIZE;

fn percentile(sorted: &[f64], p: f64) -> f64 {
    let idx = ((p / 100.0) * (sorted.len() as f64 - 1.0)).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

fn rss_mb() -> f64 {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::System::ProcessStatus::{
            GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
        };
        let handle = windows_sys::Win32::System::Threading::GetCurrentProcess();
        let mut pmc = PROCESS_MEMORY_COUNTERS {
            cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
            PageFaultCount: 0,
            PeakWorkingSetSize: 0,
            WorkingSetSize: 0,
            QuotaPeakPagedPoolUsage: 0,
            QuotaPagedPoolUsage: 0,
            QuotaPeakNonPagedPoolUsage: 0,
            QuotaNonPagedPoolUsage: 0,
            PagefileUsage: 0,
            PeakPagefileUsage: 0,
        };
        if GetProcessMemoryInfo(handle, &mut pmc, pmc.cb) != 0 {
            return pmc.WorkingSetSize as f64 / 1024.0 / 1024.0;
        }
        0.0
    }
    #[cfg(not(windows))]
    {
        0.0
    }
}

fn cpu_times_secs() -> f64 {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::Foundation::FILETIME;
        use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
        let handle = GetCurrentProcess();
        let zero = FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        };
        let (mut creation, mut exit, mut kernel, mut user) = (zero, zero, zero, zero);
        let ft = |f: &FILETIME| (f.dwHighDateTime as u64) << 32 | f.dwLowDateTime as u64;
        if GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user) != 0 {
            (ft(&kernel) + ft(&user)) as f64 / 10_000_000.0
        } else {
            0.0
        }
    }
    #[cfg(not(windows))]
    {
        0.0
    }
}

fn wal_bytes(db_path: &str) -> u64 {
    std::fs::metadata(format!("{db_path}-wal"))
        .map(|m| m.len())
        .unwrap_or(0)
}

/// 真实形态的混合事件（与 perf-write 相同分布；press/switch 会触发 agg 增量维护）
fn make_event(i: usize, base_ts: chrono::DateTime<chrono::Utc>) -> Event {
    let ts = base_ts + chrono::Duration::milliseconds(i as i64);
    let mut ev = match i % 5 {
        0 | 1 => Event::new(EventAction::Move, EventType::Mouse)
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

fn flush_batch(db: &Database, batch: &[Event], written: &AtomicUsize) {
    db.insert_events(batch);
    written.fetch_add(batch.len(), Ordering::Relaxed);
    db.update_agg(batch); // 真实 writer 路径含 agg 增量维护
}

fn main() {
    let dur_secs: u64 = std::env::var("KYNOPTIC_PERF3_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1800);
    let db_path = "target/perf3-longrun.db".to_string();
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{db_path}{suffix}"));
    }
    let db = Arc::new(Database::open(&db_path).expect("open db"));

    // ════ P1: agg 维护成本 vs agg_minute 规模 ════
    println!("== P1: update_agg cost vs agg_minute size ==");
    // 注意：P1 用独立的临时库——它对同批事件重复 update_agg 且不落 events，
    // 会污染主库的 agg 一致性校验。
    let p1_path = "target/perf3-p1.db".to_string();
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{p1_path}{suffix}"));
    }
    let db_p1 = Database::open(&p1_path).expect("open p1 db");
    let batch: Vec<Event> = (0..FLUSH)
        .map(|i| make_event(i, chrono::Utc::now()))
        .collect();
    let time_update_agg = |db: &Database, batch: &[Event]| -> f64 {
        let t = Instant::now();
        db.update_agg(batch);
        t.elapsed().as_secs_f64() * 1000.0
    };
    let mut runs_empty = Vec::new();
    for _ in 0..20 {
        runs_empty.push(time_update_agg(&db_p1, &batch));
    }
    runs_empty.sort_by(|a, b| a.partial_cmp(b).unwrap());
    // 造 525k 行 agg_minute（题设"一年"规模）：真实日期序列保证 (date,hour,minute,bucket) 唯一
    db_p1.with_writer(
        |conn| {
            conn.execute_batch("BEGIN IMMEDIATE;").unwrap();
            {
                let mut stmt = conn
                    .prepare_cached(
                        "INSERT INTO agg_minute (date,hour,minute,bucket_id,sum_value,count_value)
                         VALUES (?1,?2,?3,?4,1,1)",
                    )
                    .unwrap();
                let start_day = chrono::NaiveDate::from_ymd_opt(2024, 1, 1).unwrap();
                for m in 0..525_000usize {
                    let day = m / 1440;
                    let mi = m % 1440;
                    let date = (start_day + chrono::Duration::days(day as i64))
                        .format("%Y-%m-%d")
                        .to_string();
                    stmt.execute(rusqlite::params![
                        date,
                        (mi / 60) as i64,
                        (mi % 60) as i64,
                        [
                            "input_keys",
                            "input_clicks",
                            "input_moves",
                            "window_switches"
                        ][mi % 4]
                    ])
                    .unwrap();
                }
            }
            conn.execute_batch("COMMIT;").unwrap();
        },
        || {},
    );
    let agg_rows: i64 = db_p1
        .reader()
        .query_row("SELECT COUNT(*) FROM agg_minute", [], |r| r.get(0))
        .unwrap_or(0);
    let mut runs_big = Vec::new();
    for _ in 0..20 {
        runs_big.push(time_update_agg(&db_p1, &batch));
    }
    drop(db_p1);
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{p1_path}{suffix}"));
    }
    runs_big.sort_by(|a, b| a.partial_cmp(b).unwrap());
    println!(
        r#"{{"bench":"P1_agg_cost","batch_size":{FLUSH},"update_agg_ms_median_empty_agg":{:.3},"agg_minute_rows":{agg_rows},"update_agg_ms_median_525k_agg":{:.3},"per_event_ns_empty":{:.0},"per_event_ns_525k":{:.0}}}"#,
        runs_empty[runs_empty.len() / 2],
        runs_big[runs_big.len() / 2],
        runs_empty[runs_empty.len() / 2] * 1e6 / FLUSH as f64,
        runs_big[runs_big.len() / 2] * 1e6 / FLUSH as f64,
    );

    // ════ P2: 持续写（真实路径）+ RSS/WAL 采样 + 并发读者 ════
    println!("== P2: sustained write, {dur_secs}s, with concurrent readers ==");
    let written = Arc::new(AtomicUsize::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let (txq, rxq) = bounded::<Event>(CHANNEL_CAPACITY);
    let db_w = db.clone();
    let w2 = written.clone();
    let s_w = stop.clone();
    let writer_handle = std::thread::spawn(move || {
        let mut batch: Vec<Event> = Vec::with_capacity(FLUSH);
        let mut last_flush = Instant::now();
        loop {
            while batch.len() < FLUSH {
                match rxq.recv_timeout(Duration::from_millis(100)) {
                    Ok(e) => batch.push(e),
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => break,
                    Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                        if !batch.is_empty() {
                            flush_batch(&db_w, &batch, &w2);
                        }
                        return;
                    }
                }
            }
            if batch.len() >= FLUSH
                || (!batch.is_empty() && last_flush.elapsed() >= Duration::from_secs(1))
            {
                flush_batch(&db_w, &batch, &w2);
                batch.clear();
                last_flush = Instant::now();
            }
            if s_w.load(Ordering::Acquire) && rxq.is_empty() {
                if !batch.is_empty() {
                    flush_batch(&db_w, &batch, &w2);
                }
                return;
            }
        }
    });

    // 加速合成进料：目标 ~2000 事件/s（重度真实负载上界，真实人手 <30/s）
    let feed_handle = {
        let txq = txq.clone();
        let stop = stop.clone();
        std::thread::spawn(move || {
            let mut i = 0usize;
            let base = chrono::Utc::now();
            let t0 = Instant::now();
            while !stop.load(Ordering::Acquire) {
                let ev = make_event(i, base + chrono::Duration::milliseconds(i as i64));
                let _ = txq.try_send(ev);
                i += 1;
                if i.is_multiple_of(200) {
                    let target_ms = (i as u64) / 2; // 2000/s
                    let now_ms = t0.elapsed().as_millis() as u64;
                    if target_ms > now_ms {
                        std::thread::sleep(Duration::from_millis(target_ms - now_ms));
                    }
                }
            }
            drop(txq);
            i
        })
    };

    // P4 并发读者负载（writer 工作期间持续查询；WAL 下读者不应阻塞 writer）
    let db4 = db.clone();
    let stop4 = stop.clone();
    let reader_queries = Arc::new(AtomicU64::new(0));
    let reader_total_us = Arc::new(AtomicU64::new(0));
    {
        let rq = reader_queries.clone();
        let rt = reader_total_us.clone();
        std::thread::spawn(move || {
            while !stop4.load(Ordering::Acquire) {
                let t = Instant::now();
                let r = db4.reader();
                let n1: i64 = r
                    .query_row("SELECT COUNT(*) FROM events", [], |x| x.get(0))
                    .unwrap_or(-1);
                let switches: i64 = r
                    .query_row(
                        "SELECT COALESCE(SUM(sum_value),0) FROM agg_minute WHERE bucket_id='window_switches'",
                        [],
                        |x| x.get(0),
                    )
                    .unwrap_or(-1);
                if n1 >= 0 && switches >= 0 && (n1 == 0) != (switches == 0) {
                    log::warn!("P4: 可疑的不一致快照 n1={n1} switches={switches}");
                }
                drop(r);
                rq.fetch_add(1, Ordering::Relaxed);
                rt.fetch_add(t.elapsed().as_micros() as u64, Ordering::Relaxed);
                std::thread::sleep(Duration::from_millis(5));
            }
        });
    }

    // 采样循环
    let t0 = Instant::now();
    let k0 = cpu_times_secs();
    let mut samples: Vec<(f64, f64, u64)> = Vec::new(); // (hours, rss_mb, wal_bytes)
    let start_rss = rss_mb();
    let mut reader_samples: Vec<f64> = Vec::new(); // 每 5s 窗口读者平均延迟 ms
    let mut last_rq = 0u64;
    let mut last_rt = 0u64;
    while t0.elapsed().as_secs() < dur_secs {
        std::thread::sleep(Duration::from_secs(5));
        samples.push((
            t0.elapsed().as_secs_f64() / 3600.0,
            rss_mb(),
            wal_bytes(&db_path),
        ));
        let rq = reader_queries.load(Ordering::Relaxed);
        let rt = reader_total_us.load(Ordering::Relaxed);
        if rq > last_rq {
            reader_samples.push((rt - last_rt) as f64 / (rq - last_rq) as f64 / 1000.0);
        }
        last_rq = rq;
        last_rt = rt;
    }
    let k1 = cpu_times_secs();
    stop.store(true, Ordering::Release);
    let fed = feed_handle.join().unwrap_or(0);
    drop(txq);
    writer_handle.join().unwrap();
    let total = written.load(Ordering::Relaxed);
    let write_hours = t0.elapsed().as_secs_f64() / 3600.0;
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(12) as f64;
    let cpu_pct = (k1 - k0) / write_hours / cores * 100.0;

    let n = samples.len() as f64;
    let (sx, sy, sxx, sxy) = samples
        .iter()
        .fold((0.0, 0.0, 0.0, 0.0), |(sx, sy, sxx, sxy), s| {
            (sx + s.0, sy + s.1, sxx + s.0 * s.0, sxy + s.0 * s.1)
        });
    let slope = if n > 1.0 {
        (n * sxy - sx * sy) / (n * sxx - sx * sx)
    } else {
        0.0
    };
    let rss_max = samples.iter().map(|s| s.1).fold(0.0f64, f64::max);
    let wal_max = samples.iter().map(|s| s.2).fold(0u64, u64::max);
    let wal_min = samples.iter().map(|s| s.2).min().unwrap_or(0);
    let db_main = std::fs::metadata(&db_path).map(|m| m.len()).unwrap_or(0);
    let per_event = (db_main + wal_max) as f64 / total.max(1) as f64;
    let write_throughput = total as f64 / t0.elapsed().as_secs_f64();
    reader_samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
    println!(
        r#"{{"bench":"P2_sustained","window_secs":{},"events_written":{},"feed_attempts":{},"write_throughput_eps":{:.0},"rss_start_mb":{:.2},"rss_max_mb":{:.2},"rss_slope_mb_per_hour":{:.3},"cpu_pct_of_one_core":{:.3},"wal_min_mb":{:.2},"wal_max_mb":{:.2},"bytes_per_event_incl_agg":{:.0}}}"#,
        dur_secs,
        total,
        fed,
        write_throughput,
        start_rss,
        rss_max,
        slope,
        cpu_pct,
        wal_min as f64 / 1e6,
        wal_max as f64 / 1e6,
        per_event,
    );
    println!(
        r#"{{"bench":"P4_concurrent_readers","queries":{},"reader_p50_ms":{:.2},"reader_p95_ms":{:.2},"note":"concurrent read load active during entire write window"}}"#,
        reader_queries.load(Ordering::Relaxed),
        percentile(&reader_samples, 50.0),
        percentile(&reader_samples, 95.0),
    );

    // 一致性校验：writer 增量维护的 agg 必须与 events 全量重建逐行一致
    // （P1 阶段会对同批事件重复 update_agg，故用"重建前后指纹相等"判定，
    // 而非直接对 events 计数——指纹相等即增量维护无丢失/无重复）。
    let fingerprint = |db: &Database| -> String {
        let r = db.reader();
        let mut stmt = r
            .prepare(
                "SELECT date||'|'||hour||'|'||minute||'|'||bucket_id||'|'||COALESCE(sum_value,0)||'|'||COALESCE(count_value,0)
                 FROM agg_minute ORDER BY date, hour, minute, bucket_id",
            )
            .expect("fingerprint prepare");
        stmt.query_map([], |row| row.get::<_, String>(0))
            .expect("fingerprint query")
            .filter_map(|x| x.ok())
            .collect::<Vec<_>>()
            .join(";")
    };
    {
        let before = fingerprint(&db);
        let removed = db.rebuild_agg();
        let after = fingerprint(&db);
        if before != after {
            let (a, b) = (before.as_bytes(), after.as_bytes());
            let mut i = 0;
            while i < a.len().min(b.len()) && a[i] == b[i] {
                i += 1;
            }
            let lo = i.saturating_sub(80);
            eprintln!(
                "P4 DIFF at byte {i}:
 INCR ...{}
 REBL ...{}",
                String::from_utf8_lossy(&a[lo..(i + 120).min(a.len())]),
                String::from_utf8_lossy(&b[lo..(i + 120).min(b.len())]),
            );
        }
        println!(
            r#"{{"bench":"P4_consistency","incremental_equals_full_rebuild":{},"agg_minute_rows":{}}}"#,
            before == after,
            removed
        );
    }
    let journal: String = {
        let r = db.reader();
        r.query_row("PRAGMA journal_mode", [], |x| x.get(0))
            .unwrap_or_default()
    };
    println!(r#"{{"bench":"P4_wal_mode","reader_journal_mode":"{journal}"}}"#);

    // ════ P3: burst + 写入停滞最坏内存 ════
    println!("== P3: burst / stalled-writer worst case ==");
    // writer 不 drain = 无限磁盘停顿。hook 侧真实语义：send_event → try_send（满即丢）。
    let (txb, rxb) = bounded::<Event>(CHANNEL_CAPACITY);
    let rss_before = rss_mb();
    let mut dropped = 0usize;
    let mut accepted = 0usize;
    let mut max_enqueue_ns = 0u128;
    let mut total_ns = 0u128;
    let base = chrono::Utc::now();
    for i in 0..(CHANNEL_CAPACITY * 3) {
        let ev = make_event(i, base + chrono::Duration::milliseconds(i as i64));
        let t = Instant::now();
        // 与 collector::send_event 完全相同的语义
        match txb.try_send(ev) {
            Ok(()) => accepted += 1,
            Err(crossbeam_channel::TrySendError::Full(_)) => dropped += 1,
            Err(_) => {}
        }
        let ns = t.elapsed().as_nanos();
        total_ns += ns;
        if ns > max_enqueue_ns {
            max_enqueue_ns = ns;
        }
    }
    std::thread::sleep(Duration::from_millis(200));
    let rss_settled = rss_mb();
    println!(
        r#"{{"bench":"P3_burst_stall","channel_capacity":{CHANNEL_CAPACITY},"burst_attempts":{},"accepted":{},"dropped":{},"enqueue_total_ms":{:.1},"enqueue_mean_ns":{:.0},"enqueue_max_ns":{max_enqueue_ns},"queue_rss_delta_mb":{:.2}}}"#,
        CHANNEL_CAPACITY * 3,
        accepted,
        dropped,
        total_ns as f64 / 1e6,
        total_ns as f64 / (CHANNEL_CAPACITY * 3) as f64,
        rss_settled - rss_before,
    );
    // 排空（磁盘恢复）：测追赶速度
    let t_drain = Instant::now();
    let mut drained = 0usize;
    let mut batch: Vec<Event> = Vec::with_capacity(FLUSH);
    while drained < accepted {
        match rxb.try_recv() {
            Ok(e) => {
                batch.push(e);
                drained += 1;
                if batch.len() >= FLUSH {
                    flush_batch(&db, &batch, &AtomicUsize::new(0));
                    batch.clear();
                }
            }
            Err(_) => break,
        }
    }
    if !batch.is_empty() {
        flush_batch(&db, &batch, &AtomicUsize::new(0));
    }
    println!(
        r#"{{"bench":"P3_drain_after_stall","drained":{drained},"drain_ms":{:.1},"drain_eps":{:.0}}}"#,
        t_drain.elapsed().as_millis(),
        drained as f64 / t_drain.elapsed().as_secs_f64().max(1e-9),
    );
    let _ = rxb;

    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{db_path}{suffix}"));
    }
}
