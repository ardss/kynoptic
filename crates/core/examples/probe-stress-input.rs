//! probe-stress-input — Hook 稳态观测（被动版，无任何合成输入）
//!
//! ⚠ 历史版本曾用 SendInput 注入合成输入做 60s 压力测试——**该模式已移除**
//! （会直接干扰真实桌面会话，见 CODE_NOTES §10 的 DEFERRED 记录）。
//! 现版本完全被动：启动完整默认采集器，观测真实使用下的事件速率与 RSS，
//! 用于验证 hook 线程存活与无事件丢失趋势。事件量取决于真实操作，
//! 判读时关注"计数单调增长、无断流、RSS 平稳"，不追求固定速率。
//!
//! 运行：`cargo run --release -p kynoptic-core --example probe-stress-input`
//! （KYNOPTIC_STRESS_SECS 覆盖时长，默认 60；KYNOPTIC_DB 指定临时库）

use std::time::{Duration, Instant};

use kynoptic_core::collector;

#[cfg(windows)]
mod sample {
    use windows_sys::Win32::System::ProcessStatus::{
        GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    pub fn rss_bytes() -> u64 {
        unsafe {
            let handle = GetCurrentProcess();
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
                pmc.WorkingSetSize as u64
            } else {
                0
            }
        }
    }
}

fn main() {
    #[cfg(not(windows))]
    {
        eprintln!("仅支持 Windows");
        return;
    }
    #[cfg(windows)]
    {
        let secs: u64 = std::env::var("KYNOPTIC_STRESS_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(60);
        let db_path = std::env::var("KYNOPTIC_DB")
            .unwrap_or_else(|_| format!("target/probe-stress-{}.db", std::process::id()));

        // flush=1s：事件近实时可见，末窗存活检查才有意义
        let settings = collector::CollectorSettings {
            write_flush_interval_secs: 1,
            ..Default::default()
        };
        let enabled: std::collections::HashSet<String> =
            kynoptic_core::registry::default_enabled_ids()
                .into_iter()
                .map(|s| s.to_string())
                .collect();
        let mut collector = collector::start_collection_custom(&enabled, settings, &db_path);

        let start = Instant::now();
        let rss0 = sample::rss_bytes();
        let mut peak = rss0;
        while start.elapsed().as_secs() < secs {
            std::thread::sleep(Duration::from_secs(5));
            let rss = sample::rss_bytes();
            peak = peak.max(rss);
            let conn = collector.db.reader();
            let n: i64 = conn
                .query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))
                .unwrap_or(0);
            let t = start.elapsed().as_secs();
            println!(
                "t={:>3}s  events={:<7}  eps={:.1}  rss={:.1}MB",
                t,
                n,
                n as f64 / t.max(1) as f64,
                rss as f64 / 1_048_576.0
            );
        }
        collector.shutdown();

        let conn = collector.db.reader();
        let total: i64 = conn
            .query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))
            .unwrap_or(0);
        let tail: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM events WHERE timestamp >= ?",
                [(chrono::Utc::now() - chrono::Duration::seconds(6)).to_rfc3339()],
                |r| r.get(0),
            )
            .unwrap_or(0);
        let by_type: Vec<(String, i64)> = conn
            .prepare("SELECT event_type, COUNT(*) FROM events GROUP BY event_type ORDER BY 2 DESC")
            .unwrap()
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
            .unwrap()
            .flatten()
            .collect();
        let rss1 = sample::rss_bytes();

        println!("\n=== probe-stress（被动观测，{}s）===", secs);
        println!("events_written:     {}", total);
        println!(
            "events_per_sec_avg: {:.1}（取决于真实使用强度）",
            total as f64 / start.elapsed().as_secs_f64()
        );
        println!(
            "rss_mb start/end/peak: {:.1} / {:.1} / {:.1}",
            rss0 as f64 / 1_048_576.0,
            rss1 as f64 / 1_048_576.0,
            peak as f64 / 1_048_576.0
        );
        println!("events_in_last_6s:  {}（hook 存活：{}）", tail, tail > 0);
        println!("events_by_type: {:?}", by_type);
    }
}
