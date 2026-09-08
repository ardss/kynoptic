//! perf-startup — 启动延迟基准（STARTUP bench）
//!
//! 度量：进程 spawn → 全部 monitor「首次采集完成」的耗时。中位数取 10 次。
//! 实现：本二进制以 --child 模式重启自身，父进程读子进程 stderr，
//! 等待 N 条 "首次采集完成"（N = monitor 数）后计时并杀掉子进程。
//!
//! 运行：`cargo run --release -p kynoptic-core --example perf-startup`

use std::io::BufRead;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const RUNS: usize = 10;

fn main() {
    if std::env::args().any(|a| a == "--child") {
        // 子进程：跑采集器 10 秒（stderr 输出采集日志）
        let db =
            std::env::var("KYNOPTIC_DB").unwrap_or_else(|_| "target/perf-startup-child.db".into());
        let mut c = kynoptic_core::collector::start_collection(&db);
        std::thread::sleep(Duration::from_secs(10));
        c.shutdown();
        return;
    }

    let exe = std::env::current_exe().expect("current exe");
    let mut results: Vec<f64> = Vec::new();
    for run in 0..RUNS {
        let db = format!("target/perf-startup-{run}.db");
        let _ = std::fs::remove_file(&db);
        let t0 = Instant::now();
        let mut child = Command::new(&exe)
            .arg("--child")
            .env("KYNOPTIC_DB", &db)
            .env("RUST_LOG", "info")
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn child");

        // 等 12 个 monitor 的 "首次采集完成"
        let stderr = child.stderr.take().expect("stderr");
        let mut first_done: Option<Instant> = None;
        let mut count = 0;
        for line in std::io::BufReader::new(stderr).lines() {
            let Ok(line) = line else { break };
            if line.contains("首次采集完成") {
                count += 1;
                if first_done.is_none() {
                    first_done = Some(Instant::now());
                }
                if count >= 12 {
                    break;
                }
            }
        }
        let elapsed = t0.elapsed().as_secs_f64() * 1000.0;
        let _ = child.kill();
        let _ = child.wait();
        let _ = std::fs::remove_file(&db);
        results.push(elapsed);
        println!("run {} first-collection: {:.1} ms", run + 1, elapsed);
    }

    let mut s = results.clone();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let median = s[s.len() / 2];
    let min = s[0];
    let max = s[s.len() - 1];
    println!(
        r#"{{"bench":"perf-startup","runs":{RUNS},"median_ms":{median:.1},"min_ms":{min:.1},"max_ms":{max:.1}}}"#
    );
}
