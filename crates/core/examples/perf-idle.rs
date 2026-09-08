//! perf-idle — 采集器空载开销基准（IDLE OVERHEAD bench）
//!
//! 在本进程内启动完整采集器（全部原生 monitor/hook + writer 线程），
//! 每 1s 采样一次（默认持续 600s，可用 KYNOPTIC_PERF_SECS 覆盖）：
//! - 进程 CPU%（归一化到单核：GetProcessTimes 增量 / 墙钟增量）
//! - 私有工作集（GetProcessMemoryInfo）
//! - 累计磁盘写入字节（GetProcessIoCounters）
//!
//! 结束后输出 avg / p95 / max、RSS 线性回归斜率（MB/hour，判断单调增长）、
//! 事件数与 DB 字节数。结果打印为 JSON，供 BENCHMARKS.md 引用。
//!
//! 运行：`cargo run --release -p kynoptic-core --example perf-idle`
//! （KYNOPTIC_DB 指定临时库；KYNOPTIC_PERF_SECS 覆盖时长，冒烟用小值）

use std::time::{Duration, Instant};

use kynoptic_core::collector;

#[cfg(windows)]
mod sample {
    use windows_sys::Win32::Foundation::{CloseHandle, FILETIME};
    use windows_sys::Win32::System::ProcessStatus::{
        GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
    };
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcess, GetCurrentProcessId, GetProcessIoCounters, GetProcessTimes, IO_COUNTERS,
    };

    /// (cpu_seconds_total, rss_bytes, io_write_bytes)
    pub fn sample() -> (f64, u64, u64) {
        unsafe {
            let handle = GetCurrentProcess();

            // CPU 时间（FILETIME，100ns 单位）
            let zero = FILETIME {
                dwLowDateTime: 0,
                dwHighDateTime: 0,
            };
            let (mut create, mut exit, mut kernel, mut user) = (zero, zero, zero, zero);
            let _ = GetProcessTimes(handle, &mut create, &mut exit, &mut kernel, &mut user);
            let ft = |f: &FILETIME| (f.dwHighDateTime as u64) << 32 | f.dwLowDateTime as u64;
            let cpu_secs = (ft(&kernel) + ft(&user)) as f64 / 10_000_000.0;

            // 工作集
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
            let rss = if GetProcessMemoryInfo(handle, &mut pmc, pmc.cb) != 0 {
                pmc.WorkingSetSize as u64
            } else {
                0
            };

            // 累计磁盘写入
            let mut io = IO_COUNTERS {
                ReadOperationCount: 0,
                ReadTransferCount: 0,
                WriteOperationCount: 0,
                WriteTransferCount: 0,
                OtherOperationCount: 0,
                OtherTransferCount: 0,
            };
            let disk_w = if GetProcessIoCounters(handle, &mut io) != 0 {
                io.WriteTransferCount
            } else {
                0
            };

            let _ = CloseHandle(handle); // pseudo-handle：CloseHandle 返回错误但无副作用
            (cpu_secs, rss, disk_w)
        }
    }

    pub fn pid() -> u32 {
        unsafe { GetCurrentProcessId() }
    }
}

#[cfg(not(windows))]
mod sample {
    pub fn sample() -> (f64, u64, u64) {
        (0.0, 0, 0)
    }
    pub fn pid() -> u32 {
        0
    }
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = ((p / 100.0) * (sorted.len() as f64 - 1.0)).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

fn stats(v: &[f64]) -> (f64, f64, f64) {
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let avg = v.iter().sum::<f64>() / v.len() as f64;
    (avg, percentile(&s, 95.0), *s.last().unwrap_or(&0.0))
}

/// 最小二乘斜率（x = 秒，y = 任意 f64 序列），返回每小时的增量
fn slope_per_hour(x: &[f64], y: &[f64]) -> f64 {
    let n = x.len() as f64;
    let (sx, sy): (f64, f64) = (x.iter().sum(), y.iter().sum());
    let sxx: f64 = x.iter().map(|t| t * t).sum();
    let sxy: f64 = x.iter().zip(y).map(|(t, v)| t * v).sum();
    let denom = n * sxx - sx * sx;
    if denom.abs() < f64::EPSILON {
        return 0.0;
    }
    let slope = (n * sxy - sx * sy) / denom; // 单位/秒
    slope * 3600.0
}

fn db_stat(conn: &rusqlite::Connection, sql: &str) -> i64 {
    conn.query_row(sql, [], |r| r.get::<_, i64>(0)).unwrap_or(0)
}

fn main() {
    let secs: u64 = std::env::var("KYNOPTIC_PERF_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(600);
    let db_path = std::env::var("KYNOPTIC_DB").unwrap_or_else(|_| "target/perf-idle.db".into());
    let _ = std::fs::remove_file(&db_path);

    let mut collector = collector::start_collection(&db_path);
    println!("{{\"phase\":\"started\",\"pid\":{}}}", sample::pid());

    let mut cpu_pct = Vec::new();
    let mut rss_mb = Vec::new();
    let mut disk_mb_per_h = Vec::new();
    let mut t_secs = Vec::new();

    // 前 10s 视为启动抖动期，不计入稳态窗口
    let start = Instant::now();
    let warmup = Duration::from_secs(10);
    let (mut prev_cpu, _prev_rss, mut prev_disk) = sample::sample();
    let mut prev_wall = Instant::now();
    loop {
        std::thread::sleep(Duration::from_secs(1));
        let now_wall = Instant::now();
        let (cpu, rss, disk) = sample::sample();
        let wall = now_wall.duration_since(prev_wall).as_secs_f64();
        if start.elapsed() >= warmup {
            let pct = if wall > 0.0 {
                (cpu - prev_cpu) / wall * 100.0
            } else {
                0.0
            };
            t_secs.push(start.elapsed().as_secs_f64());
            cpu_pct.push(pct);
            rss_mb.push(rss as f64 / 1024.0 / 1024.0);
            disk_mb_per_h.push(disk.saturating_sub(prev_disk) as f64 / 1024.0 / 1024.0 * 3600.0);
        }
        prev_cpu = cpu;
        prev_disk = disk;
        prev_wall = now_wall;
        if start.elapsed().as_secs() >= secs {
            break;
        }
    }

    let run_secs = start.elapsed().as_secs_f64();
    collector.shutdown();

    let (_cpu, _rss, disk) = sample::sample();
    let disk_total_mb = disk as f64 / 1024.0 / 1024.0;

    let (cpu_avg, cpu_p95, cpu_max) = stats(&cpu_pct);
    let (rss_avg, rss_p95, rss_max) = stats(&rss_mb);
    let rss_slope_mb_h = slope_per_hour(&t_secs, &rss_mb);
    let disk_slope_mb_h = slope_per_hour(&t_secs, &disk_mb_per_h);

    // DB 尺寸与事件数
    let db_bytes = std::fs::metadata(&db_path).map(|m| m.len()).unwrap_or(0);
    let wal_bytes = std::fs::metadata(format!("{db_path}-wal"))
        .map(|m| m.len())
        .unwrap_or(0);
    let (events, sessions) = {
        let conn = rusqlite::Connection::open_with_flags(
            &db_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .expect("open db for stats");
        (
            db_stat(&conn, "SELECT COUNT(*) FROM events"),
            db_stat(&conn, "SELECT COUNT(*) FROM sessions"),
        )
    };

    println!(
        r#"{{"bench":"perf-idle","window_secs":{run_secs:.0},"samples":{n},
  "cpu_pct_of_one_core":{{"avg":{cpu_avg:.3},"p95":{cpu_p95:.3},"max":{cpu_max:.3}}},
  "rss_mb":{{"avg":{rss_avg:.2},"p95":{rss_p95:.2},"max":{rss_max:.2},"slope_mb_per_hour":{rss_slope_mb_h:.3}}},
  "disk_write_mb_per_hour_avg_over_1s":{disk_slope_mb_h:.4},
  "disk_total_written_mb_incl_startup":{disk_total_mb:.3},
  "events":{events},"sessions":{sessions},
  "db_bytes":{db_bytes},"db_wal_bytes":{wal_bytes}}}"#,
        n = cpu_pct.len(),
    );
}
