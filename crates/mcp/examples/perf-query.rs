//! perf-query — 100 万事件库上的查询延迟基准（QUERY LATENCY bench）
//!
//! 1. 生成合成 DB：1,000,000 条事件、30 天分布、真实类型占比
//!    （40% mouse/move、20% keyboard/press、20% window/switch、20% system/heartbeat），
//!    走与生产一致的 schema + PRAGMA + 索引。
//! 2. 基准（各 50 次，报告 p50/p95/p99）：
//!    - `kynoptic query --from <d> --to <d> --limit 20`（子进程真实 CLI，含进程启动）
//!    - MCP get_summary（1 天窗口，metrics: keys/apps/active_minutes/focus_segments）
//!    - MCP get_timeline（7 天，hour 粒度）
//!    - MCP get_anomalies（7 天）
//!
//! 运行：`cargo run --release -p kynoptic-mcp --example perf-query`
//! KYNOPTIC_CLI 指向 kynoptic.exe（默认自动定位 target/release/kynoptic.exe）。

use std::path::PathBuf;
use std::process::Command;
use std::time::Instant;

use rusqlite::Connection;

const EVENTS: usize = 1_000_000;
const DAYS: i64 = 30;
const RUNS: usize = 50;

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

fn seed(db_path: &str) {
    if std::path::Path::new(db_path).exists() {
        println!("seed: 复用已有 {db_path}");
        return;
    }
    println!("seed: 生成 {EVENTS} 条事件到 {db_path} ...");
    let conn = Connection::open(db_path).expect("create db");
    kynoptic_core::db::apply_pragmas(&conn).unwrap();
    conn.execute_batch(kynoptic_core::db::SCHEMA).unwrap();
    kynoptic_core::db::run_migrations(&conn);

    let base = chrono::Utc::now() - chrono::Duration::days(DAYS);
    conn.execute_batch("BEGIN IMMEDIATE;").unwrap();
    {
        let mut stmt = conn
            .prepare_cached(
                "INSERT INTO events (timestamp, event_type, event_action, event_data, app_name, window_title, session_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1)",
            )
            .unwrap();
        for i in 0..EVENTS {
            // 30 天均匀分布（聚合查询的行数才是延迟主导因素）
            let day = i / (EVENTS / DAYS as usize);
            let frac = (i % (EVENTS / DAYS as usize)) as f64 / (EVENTS / DAYS as usize) as f64;
            let hour_f = 24.0 * frac;
            let ts = base
                + chrono::Duration::days(day as i64)
                + chrono::Duration::milliseconds((hour_f * 3600.0 * 1000.0) as i64);
            let ts = ts.to_rfc3339();
            match i % 5 {
                0 | 1 => stmt.execute(rusqlite::params![
                    ts, "mouse", "move",
                    format!(r#"{{"x":{}, "y":{}}}"#, 100 + i % 800, 200 + i % 600),
                    "", ""
                ]),
                2 => stmt.execute(rusqlite::params![
                    ts, "keyboard", "press",
                    r#"{\"key\":\"a\"}"#,
                    "code.exe", "main.rs - editor"
                ]),
                3 => stmt.execute(rusqlite::params![
                    ts, "window", "switch",
                    format!(r#"{{"hwnd":{}, "title":"Document", "pid":4242}}"#, 1000 + i),
                    "word.exe", "Document - Word"
                ]),
                _ => stmt.execute(rusqlite::params![
                    ts, "system", "heartbeat",
                    format!(
                        r#"{{"memory":{{"total":34359738368,"available":17179869184,"used_percent":50}},"cpu_percent":{}}}"#,
                        10 + i % 30
                    ),
                    Option::<String>::None, Option::<String>::None
                ]),
            }
            .unwrap();
            if i % 50_000 == 49_999 {
                conn.execute_batch("COMMIT; BEGIN IMMEDIATE;").unwrap();
                eprintln!("  seeded {} ...", i + 1);
            }
        }
    }
    conn.execute_batch("COMMIT;").unwrap();
    // 聚合读缓存：与生产 Database::open 的懒回填一致，get_anomalies 读
    // agg_minute/agg_daily（派生缓存；原始 events 原样保留）。
    if kynoptic_core::db::agg::backfill_if_needed(&conn) {
        println!("seed: agg 缓存回填完成");
    }
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))
        .unwrap();
    println!("seed: 完成，events={n}");
}

fn main() {
    let db_path = std::env::var("KYNOPTIC_DB").unwrap_or_else(|_| "target/perf-query-1m.db".into());
    seed(&db_path);

    // ── MCP 数据面（进程内，等价 MCP server 持连接调用）──
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

    // get_timeline 7 天 hour 粒度
    {
        let mut times = Vec::new();
        for _ in 0..RUNS {
            let t = Instant::now();
            let _ = kynoptic_mcp::state::timeline(&conn, &t7, &t0, "hour", 500).expect("timeline");
            times.push(t.elapsed().as_secs_f64() * 1000.0);
        }
        all.push(("get_timeline (7d, hour)".into(), times));
    }

    // get_anomalies 7 天
    {
        let mut times = Vec::new();
        for _ in 0..RUNS {
            let t = Instant::now();
            let _ = kynoptic_mcp::state::anomalies(&conn, 7, 50);
            times.push(t.elapsed().as_secs_f64() * 1000.0);
        }
        all.push(("get_anomalies (7d)".into(), times));
    }

    // ── CLI 子进程：kynoptic query --from --to --limit 20 ──
    let cli = std::env::var("KYNOPTIC_CLI").unwrap_or_else(|_| {
        let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        p.pop(); // crates/mcp -> crates
        p.pop(); // crates -> workspace root
        p.join("target/release/kynoptic.exe")
            .to_string_lossy()
            .into_owned()
    });
    let from = (now - chrono::Duration::days(1))
        .format("%Y-%m-%dT%H:%M:%S")
        .to_string();
    let to = now.format("%Y-%m-%dT%H:%M:%S").to_string();
    if std::path::Path::new(&cli).exists() {
        let mut times = Vec::new();
        for _ in 0..RUNS {
            let t = Instant::now();
            let out = Command::new(&cli)
                .args([
                    "query", "--from", &from, "--to", &to, "--limit", "20", "--json",
                ])
                .env("KYNOPTIC_DB", &db_path)
                .output()
                .expect("spawn kynoptic");
            times.push(t.elapsed().as_secs_f64() * 1000.0);
            assert!(out.status.success(), "kynoptic query failed");
        }
        all.push(("kynoptic query (1d, limit 20, spawn)".into(), times));
    } else {
        println!("  [skip] 未找到 CLI: {}", cli);
    }

    println!("perf-query results (n={RUNS} each):");
    for (name, times) in &all {
        report(name, times);
    }
}
