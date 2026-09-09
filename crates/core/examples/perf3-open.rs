//! perf3-open — Scenario 5: startup (Database::open) on a large existing DB.
//!
//! Measures:
//!   A. Database::open on the 1M-row DB with agg cache POPULATED (steady-state upgrade path).
//!   B. Same DB but agg_minute/agg_daily emptied first (legacy-DB first-open backfill path):
//!      measures the synchronous backfill inside Database::open (P0 if it blocks seconds).
//!   C. re-open after B (agg now populated) — confirms steady-state open is fast again.
//!
//! Env: KYNOPTIC_DB1M = path to the 1M-row DB (default target/perf3-zipf-1m.db).
//! The example works on a COPY for phase B (original restored untouched).
//!
//! Run: cargo run --release -p kynoptic-core --example perf3-open

use std::time::Instant;

fn main() {
    let src = std::env::var("KYNOPTIC_DB1M").unwrap_or_else(|_| "target/perf-zipf-1m.db".into());

    // ── A: open with agg populated ──
    let t = Instant::now();
    {
        let db = kynoptic_core::db::Database::open(&src).expect("open A");
        let open_us = t.elapsed().as_secs_f64() * 1000.0;
        let n: i64 = db
            .reader()
            .query_row("SELECT COUNT(*) FROM agg_minute", [], |r| r.get(0))
            .unwrap_or(0);
        println!(
            r#"{{"phase":"A_open_agg_populated","open_ms":{open_us:.1},"agg_minute_rows":{n}}}"#
        );
    }

    // ── B: copy → empty agg → open (legacy first-open backfill path) ──
    let copy = format!("{src}.backfilltest");
    let _ = std::fs::remove_file(&copy);
    let t = Instant::now();
    std::fs::copy(&src, &copy).expect("copy db");
    println!(
        r#"{{"phase":"B_copy","copy_ms":{:.1}}}"#,
        t.elapsed().as_secs_f64() * 1000.0
    );
    {
        let conn = rusqlite::Connection::open(&copy).expect("open copy raw");
        conn.execute_batch("DELETE FROM agg_minute; DELETE FROM agg_daily;")
            .unwrap();
        // WAL 已经物理存在，需要 checkpoint 让"agg 为空"落到主库（模拟存量库形态）
        conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);").ok();
    }
    let t = Instant::now();
    {
        let _db = kynoptic_core::db::Database::open(&copy).expect("open B");
        println!(
            r#"{{"phase":"B_open_empty_agg_backfill","open_ms":{:.1}}}"#,
            t.elapsed().as_secs_f64() * 1000.0
        );
    }
    // ── C: re-open steady state ──
    let t = Instant::now();
    {
        let _db = kynoptic_core::db::Database::open(&copy).expect("open C");
        println!(
            r#"{{"phase":"C_reopen_steady","open_ms":{:.1}}}"#,
            t.elapsed().as_secs_f64() * 1000.0
        );
    }
    let _ = std::fs::remove_file(&copy);
    let _ = std::fs::remove_file(format!("{copy}-wal"));
    let _ = std::fs::remove_file(format!("{copy}-shm"));
}
