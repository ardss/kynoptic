//! perf3-zipf — Scenario 3: query latency on a REALISTIC app distribution.
//!
//! Prior perf-query seeded 1M events on 2-3 apps (far more concentrated than real
//! usage). This bench seeds 1,000,000 events across 1,200 distinct apps with a
//! Zipf(s=1.1) popularity skew (top app ~10% of events, long tail of rare apps),
//! 30-day window ending now, realistic type mix — then measures the same MCP
//! query set as perf-query (50 runs each, p50/p95/p99) on the agg-cache path.
//!
//! Run: cargo run --release -p kynoptic-mcp --example perf3-zipf
//! DB (reused across runs): target/perf-zipf-1m.db

use std::time::Instant;

use rusqlite::Connection;

const EVENTS: usize = 1_000_000;
const DAYS: i64 = 30;
const APPS: usize = 1_200;
const RUNS: usize = 50;
const ZIPF_S: f64 = 1.1;

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = ((p / 100.0) * (sorted.len() as f64 - 1.0)).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

fn report(name: &str, times_ms: &[f64]) {
    let mut s = times_ms.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap());
    println!(
        "  {name:<44} p50={:>8.2}ms  p95={:>8.2}ms  p99={:>8.2}ms  max={:>8.2}ms",
        percentile(&s, 50.0),
        percentile(&s, 95.0),
        percentile(&s, 99.0),
        s.last().copied().unwrap_or(0.0),
    );
}

/// Zipf 权重表（app rank 1..=APPS, p ∝ 1/rank^s），返回累积权重。
fn zipf_cdf() -> Vec<f64> {
    let mut cum = Vec::with_capacity(APPS);
    let mut total = 0.0;
    for r in 1..=APPS {
        total += 1.0 / (r as f64).powf(ZIPF_S);
        cum.push(total);
    }
    for c in cum.iter_mut() {
        *c /= total;
    }
    cum
}

fn pick_app(cdf: &[f64], u: f64) -> usize {
    match cdf.binary_search_by(|p| p.partial_cmp(&u).unwrap()) {
        Ok(i) => i,
        Err(i) => i,
    }
}

fn seed(db_path: &str) {
    if std::path::Path::new(db_path).exists() {
        println!("seed: 复用已有 {db_path}（补跑迁移，确保 0004 索引到位）");
        let conn = Connection::open(db_path).expect("open existing db");
        let _ = kynoptic_core::db::run_migrations(&conn);
        return;
    }
    println!("seed: 生成 {EVENTS} 条事件（{APPS} apps, Zipf s={ZIPF_S}）到 {db_path} ...");
    let conn = Connection::open(db_path).expect("create db");
    kynoptic_core::db::apply_pragmas(&conn).unwrap();
    conn.execute_batch(kynoptic_core::db::SCHEMA).unwrap();
    let _ = kynoptic_core::db::run_migrations(&conn);

    let cdf = zipf_cdf();
    let base = chrono::Utc::now() - chrono::Duration::days(DAYS);
    conn.execute_batch("BEGIN IMMEDIATE;").unwrap();
    {
        let mut stmt = conn
            .prepare_cached(
                "INSERT INTO events (timestamp, event_type, event_action, event_data, app_name, window_title, session_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1)",
            )
            .unwrap();
        // 简单可复现的 LCG 随机源（Zipf 采样 + 类型抽取）
        let mut state: u64 = 0x9E3779B97F4A7C15;
        let mut next_u = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 11) as f64 / (1u64 << 53) as f64
        };
        for i in 0..EVENTS {
            let day = (next_u() * DAYS as f64) as usize;
            // 活跃时段 08:00~24:00（16h）内均匀
            let secs = (next_u() * 16.0 * 3600.0) as i64;
            let ts = base
                + chrono::Duration::days(day as i64)
                + chrono::Duration::hours(8)
                + chrono::Duration::seconds(secs);
            let ts = ts.to_rfc3339();
            let app_i = pick_app(&cdf, next_u());
            let app = format!("app{:04}.exe", app_i + 1);
            let u = next_u();
            let res = if u < 0.55 {
                // mouse move（无 app 归属，同真实 move 事件）
                stmt.execute(rusqlite::params![
                    ts,
                    "mouse",
                    "move",
                    format!(
                        r#"{{"x":{},"y":{}}}"#,
                        (next_u() * 1920.0) as i64,
                        (next_u() * 1080.0) as i64
                    ),
                    "",
                    ""
                ])
            } else if u < 0.75 {
                stmt.execute(rusqlite::params![
                    ts,
                    "keyboard",
                    "press",
                    r#"{\"key\":\"a\"}"#,
                    app,
                    format!("{app} window")
                ])
            } else if u < 0.9 {
                stmt.execute(rusqlite::params![
                    ts,
                    "window",
                    "switch",
                    format!(r#"{{"hwnd":{}}}"#, 1000 + app_i),
                    app,
                    format!("{app} window")
                ])
            } else {
                stmt.execute(rusqlite::params![
                    ts,
                    "system",
                    "heartbeat",
                    format!(r#"{{"cpu_percent":{}}}"#, (next_u() * 100.0) as i64),
                    Option::<String>::None,
                    Option::<String>::None
                ])
            };
            res.unwrap();
            if i % 50_000 == 49_999 {
                conn.execute_batch("COMMIT; BEGIN IMMEDIATE;").unwrap();
                eprintln!("  seeded {} ...", i + 1);
            }
        }
    }
    conn.execute_batch("COMMIT;").unwrap();
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))
        .unwrap();
    let apps: i64 = conn
        .query_row(
            "SELECT COUNT(DISTINCT app_name) FROM events WHERE app_name <> ''",
            [],
            |r| r.get(0),
        )
        .unwrap();
    println!("seed: 完成，events={n}, distinct_apps={apps}");
}

fn main() {
    let db_path = std::env::var("KYNOPTIC_DB").unwrap_or_else(|_| "target/perf-zipf-1m.db".into());
    seed(&db_path);

    // agg 读缓存：与生产 Database::open 的懒回填一致
    {
        let conn = Connection::open(&db_path).expect("open for backfill");
        kynoptic_core::db::apply_pragmas(&conn).unwrap();
        let t = Instant::now();
        if kynoptic_core::db::agg::backfill_if_needed(&conn) {
            println!(
                "seed: agg 回填完成（{:.1} ms）",
                t.elapsed().as_secs_f64() * 1000.0
            );
        } else {
            println!("seed: agg 缓存已存在");
        }
    }

    let conn = kynoptic_mcp::state::open_reader(&db_path).expect("open reader");
    let today = kynoptic_core::queries::today_local_str();
    let now = chrono::Utc::now();
    let t7 = (now - chrono::Duration::days(7)).to_rfc3339();
    let t0 = now.to_rfc3339();

    let metrics = ["keys", "apps", "active_minutes", "focus_segments"];
    let mut all: Vec<(String, Vec<f64>)> = Vec::new();
    for m in metrics {
        let mut times = Vec::new();
        for _ in 0..RUNS {
            let t = Instant::now();
            kynoptic_mcp::state::summary(&conn, &today, m).expect("summary");
            times.push(t.elapsed().as_secs_f64() * 1000.0);
        }
        all.push((format!("get_summary[{m}] (1d)"), times));
    }
    {
        let mut times = Vec::new();
        for _ in 0..RUNS {
            let t = Instant::now();
            let _ = kynoptic_mcp::state::timeline(&conn, &t7, &t0, "hour", 500).expect("timeline");
            times.push(t.elapsed().as_secs_f64() * 1000.0);
        }
        all.push(("get_timeline (7d, hour)".into(), times));
    }
    {
        let mut times = Vec::new();
        for _ in 0..RUNS {
            let t = Instant::now();
            let _ = kynoptic_mcp::state::anomalies(&conn, 7, 50);
            times.push(t.elapsed().as_secs_f64() * 1000.0);
        }
        all.push(("get_anomalies (7d)".into(), times));
    }

    println!("perf3-zipf results (n={RUNS} each, {APPS} apps Zipf s={ZIPF_S}):");
    for (name, times) in &all {
        report(name, times);
    }
}
