//! probe-stress-input — Hook 压力测试（QA probe 配套）
//!
//! 在本进程内启动完整默认采集器（12 monitor + keyboard/mouse hook + writer），
//! 用 SendInput 持续注入合成输入（鼠标移动 + 按键交替），默认 60s：
//! - 每 5s 采样 RSS / 累计事件数（DB COUNT）
//! - 结束后输出：注入次数、落库事件数（分类型）、事件/秒、RSS 首末与峰值
//!
//! 判定（probe 报告引用）：事件计数单调增长（无丢失趋势）、RSS 首末差在
//! 噪声范围（<10% 且峰值无单调爬升）、hook 全程存活（末秒仍有新事件）。
//!
//! 运行：`cargo run --release -p kynoptic-core --example probe-stress-input`
//! （KYNOPTIC_STRESS_SECS 覆盖时长；KYNOPTIC_DB 指定临时库）

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use kynoptic_core::collector;

#[cfg(windows)]
mod win {
    use std::sync::atomic::{AtomicU64, Ordering};
    use windows_sys::Win32::System::ProcessStatus::{
        GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_KEYUP,
        MOUSEEVENTF_MOVE, MOUSEINPUT, VK_SPACE,
    };

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

    fn send(inputs: &mut [INPUT]) -> u64 {
        unsafe {
            SendInput(
                inputs.len() as u32,
                inputs.as_mut_ptr(),
                std::mem::size_of::<INPUT>() as i32,
            ) as u64
        }
    }

    /// 一次 tick：10 个鼠标相对移动 + 1 组按下/抬起（合成输入密度 ~100 Hz）
    pub fn input_tick(sent: &AtomicU64) {
        let mut i = 0;
        while i < 10 {
            let mut inp = INPUT {
                r#type: INPUT_MOUSE,
                Anonymous: INPUT_0 {
                    mi: MOUSEINPUT {
                        dx: (i * 7 % 21) - 10,
                        dy: (i * 5 % 17) - 8,
                        mouseData: 0,
                        dwFlags: MOUSEEVENTF_MOVE,
                        time: 0,
                        dwExtraInfo: 0,
                    },
                },
            };
            sent.fetch_add(send(&mut [inp]), Ordering::Relaxed);
            inp = INPUT {
                r#type: INPUT_MOUSE,
                Anonymous: INPUT_0 {
                    mi: MOUSEINPUT {
                        dx: 10 - (i * 7 % 21),
                        dy: 8 - (i * 5 % 17),
                        mouseData: 0,
                        dwFlags: MOUSEEVENTF_MOVE,
                        time: 0,
                        dwExtraInfo: 0,
                    },
                },
            };
            sent.fetch_add(send(&mut [inp]), Ordering::Relaxed);
            i += 1;
        }
        let key_down = INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VK_SPACE,
                    wScan: 0,
                    dwFlags: 0,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        let mut key_up = key_down;
        key_up.Anonymous.ki.dwFlags = KEYEVENTF_KEYUP;
        sent.fetch_add(send(&mut [key_down, key_up]), Ordering::Relaxed);
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

        let sent_total = std::sync::Arc::new(AtomicU64::new(0));
        // flush=1s：事件近实时可见（默认 30s flush 会让"末窗事件数/hook 存活"检查失真）
        let settings = collector::CollectorSettings {
            write_flush_interval_secs: 1,
            ..Default::default()
        };
        let mut collector = collector::start_collection_custom(
            &kynoptic_core::registry::default_enabled_ids()
                .into_iter()
                .map(|s| s.to_string())
                .collect(),
            settings,
            &db_path,
        );

        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop2 = stop.clone();
        let sent2 = sent_total.clone();
        let injector = std::thread::spawn(move || {
            while !stop2.load(Ordering::Relaxed) {
                win::input_tick(&sent2);
                std::thread::sleep(Duration::from_millis(10));
            }
        });

        let start = Instant::now();
        let rss0 = win::rss_bytes();
        let mut peak = rss0;
        let mut samples: Vec<(u64, u64)> = Vec::new(); // (t_secs, rss_mb bytes)
        while start.elapsed().as_secs() < secs {
            std::thread::sleep(Duration::from_secs(5));
            let rss = win::rss_bytes();
            peak = peak.max(rss);
            let conn = collector.db.reader();
            let n: i64 = conn
                .query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))
                .unwrap_or(0);
            let t = start.elapsed().as_secs();
            samples.push((t, rss));
            println!(
                "t={:>3}s  events={:<7}  eps={:.0}  rss={:.1}MB",
                t,
                n,
                n as f64 / t.max(1) as f64,
                rss as f64 / 1_048_576.0
            );
        }
        stop.store(true, Ordering::Relaxed);
        let _ = injector.join();
        // 注入停止后静默 3s 再收尾：末窗事件全部落库
        std::thread::sleep(Duration::from_secs(3));
        collector.shutdown();

        let conn = collector.db.reader();
        let total: i64 = conn
            .query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))
            .unwrap_or(0);
        let mut by_type: Vec<(String, i64)> = conn
            .prepare("SELECT event_type, COUNT(*) FROM events GROUP BY event_type ORDER BY 2 DESC")
            .unwrap()
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
            .unwrap()
            .flatten()
            .collect();
        by_type.sort_by(|a, b| b.1.cmp(&a.1));
        let rss1 = win::rss_bytes();
        let elapsed = start.elapsed().as_secs_f64();

        // 末 5s 窗口是否有新事件（hook 存活性）
        let tail: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM events WHERE timestamp >= ?",
                [(chrono::Utc::now() - chrono::Duration::seconds(6)).to_rfc3339()],
                |r| r.get(0),
            )
            .unwrap_or(0);

        println!("\n=== probe-stress-input 结果 ({}s) ===", secs);
        println!(
            "injected_inputs:      {}",
            sent_total.load(Ordering::Relaxed)
        );
        println!("events_written:       {}", total);
        println!("events_per_sec_avg:   {:.1}", total as f64 / elapsed);
        for (t, r) in &samples {
            println!("  sample t={}s rss={:.1}MB", t, *r as f64 / 1_048_576.0);
        }
        println!(
            "rss_mb start/end/peak: {:.1} / {:.1} / {:.1} (delta {:+.1}%)",
            rss0 as f64 / 1_048_576.0,
            rss1 as f64 / 1_048_576.0,
            peak as f64 / 1_048_576.0,
            (rss1 as f64 - rss0 as f64) / rss0 as f64 * 100.0
        );
        println!("events_in_last_6s:    {} (hook 存活: {})", tail, tail > 0);
        println!("events_by_type: {:?}", by_type);
    }
}
