//! ETW 增强模式运行时（0.8.1 A2/A3）：设置开关 → NetEtw 会话 → 分钟聚合 →
//! net_process_minute 落库，档位消费（Basic 64KB/30s ↔ 深诊 256KB/5s）。
//!
//! 此前 net_etw 模块与读路径齐全但无运行时调用方（审查 P0：表恒空、
//! /api/net-process 恒 Unavailable、诊断模式档位零消费方）。本模块是唯一
//! 生产接线点：collector 在 `net_etw_enabled` 时 spawn 本循环。
//!
//! 口径：
//! - 会话名 KynopticNetMon（与 m2_transfer_verify 隔离会话名区分）；
//!   AccessDenied（非管理员）时 quality=Unavailable 留档，线程保持存活
//!   周期重试（用户提权重启托盘前不静默假成功）。
//! - 缓冲 KB 在会话建立时按当前档位定（会话存续期不重建）；采样节奏
//!   （drain 周期）每轮实时读档位——诊断开启后 5s 一收。
//! - 诊断巡检（到期/预算强制回退）也挂在本循环（有 self_probe 的
//!   RSS/CPU 实测喂 poll，与 /api/status 同控制器实例）。

use super::net_etw::{
    EventSink, MinuteAggregator, NetEtwMonitor, ProcessIdentityCache, RealEtw, RealProcSource,
};
use crate::db::Database;
use crate::diag_mode::{controller_poll, current_profile};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// 采样自身进程供预算巡检（RSS MiB + 单核 CPU%）。复用 self_probe 的差分。
struct BudgetProbe {
    cpu_time: u64,
    wall_ms: u64,
}

impl BudgetProbe {
    fn new() -> Self {
        Self {
            cpu_time: 0,
            wall_ms: 0,
        }
    }
    /// 返回 (rss_mib, cpu_percent_single_core)。
    fn read(&mut self) -> (f64, f64) {
        let (cpu_time, wall_ms, _, s) =
            super::self_probe::read_self_sample(self.cpu_time, self.wall_ms);
        self.cpu_time = cpu_time;
        self.wall_ms = wall_ms;
        (s.working_set_mb, s.cpu_percent.unwrap_or(0.0))
    }
}

fn host_id() -> String {
    std::env::var("COMPUTERNAME").unwrap_or_else(|_| "unknown".into())
}

/// 运行时主循环（阻塞；`shutdown` 置位后收口会话并返回）。
/// 单独提函数便于在 panic 防护内调用与未来注入测试。
pub fn etw_runtime_loop(db: &Arc<Database>, shutdown: &AtomicBool) {
    let host = host_id();
    let agg: Arc<Mutex<MinuteAggregator>> = Arc::new(Mutex::new(MinuteAggregator::default()));
    let mut mon = NetEtwMonitor::new(RealEtw::with_session_name("KynopticNetMon"));
    // 缓冲按启动时档位定（会话存续期不重建；深诊缓冲在下次会话生效）
    mon.backend_mut().buffer_kb = current_profile().etw_buffer_kb;
    let sink: EventSink = {
        let agg = agg.clone();
        Arc::new(move |records| {
            if let Ok(mut g) = agg.lock() {
                for r in records {
                    g.add(&r);
                }
            }
        })
    };
    match mon.start(sink) {
        Ok(()) => log::info!(
            "ETW 增强模式已启动（会话 KynopticNetMon，缓冲 {}KB， recovered_residual={}）",
            mon.backend_ref().buffer_kb,
            mon.recovered_residual()
        ),
        Err(e) => log::warn!(
            "ETW 会话启动失败（需管理员权限；无权限期间 net_process_minute 恒空、归因面 Unavailable）: {e:?}"
        ),
    }
    let mut cache = ProcessIdentityCache::default();
    let mut probe = BudgetProbe::new();
    loop {
        let interval = current_profile().sampling_interval_ms;
        // 1 秒切片查停机（口径同其他采集线程）
        for _ in 0..(interval / 1000).max(1) {
            if shutdown.load(Ordering::Acquire) {
                let _ = mon.stop();
                return;
            }
            std::thread::sleep(Duration::from_secs(1));
        }
        // 诊断巡检（预算超限/到期自动回 Basic；喂 self_probe 实测）
        let (rss, cpu) = probe.read();
        let _ = controller_poll(chrono::Utc::now().timestamp_millis(), rss, cpu);
        // 身份缓存刷新（60s 粒度足够：桶归属区间远粗于快照间隔）
        cache.refresh(
            &mut RealProcSource::new(),
            chrono::Utc::now().timestamp() as u64,
        );
        let lost_seen = mon.backend_ref().loss_stats();
        let points = {
            let mut g = match agg.lock() {
                Ok(g) => g,
                Err(e) => e.into_inner(),
            };
            g.drain_metric_points(
                &host,
                lost_seen.1,
                lost_seen.0,
                chrono::Utc::now().timestamp_millis(),
                &mut |pid, ts| cache.resolve(pid, ts).map(|(n, e)| (n.to_string(), e)),
            )
        };
        if !points.is_empty() {
            db.insert_net_process_minute_points(&points);
        }
    }
}
