//! 自体遥测采样（0.8.1 A1）：采集进程自身的 CPU/内存/线程/句柄 → MetricPoint。
//!
//! 动机：自身资源消耗此前只能靠 process_snapshot 的全机 Top5 旁路采样偶尔
//! 看到（10 月内存 194→350MB 爬升一个月才被发现）。本模块由 collector 的
//! SelfMetrics 线程每 30s 调 [`sample_self`]，分钟桶滚动时出站落库。
//!
//! 口径：
//! - CPU%：GetProcessTimes 差分（全核累计除以逻辑核数归一，占满 1 核=100），
//!   与 monitors/process.rs 的 process_snapshot 同口径；首拍无基线不出值
//!   （不出 estimated——「估算不是实测」）。
//! - working_set_mb：GetProcessMemoryInfo WorkingSetSize。
//! - threads：CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD) 归属本 pid 计数。
//! - handles：GetProcessHandleCount。
//!
//! 测试红线：时间注入——CPU 差分只依赖传入的 prev/当前值，不出站点
//! 独立读真实时钟。

use crate::metrics::{
    MetricPoint, MetricQuality, MetricScope, MetricSource, MetricUnit, ProcessIdentity,
    METRIC_SCHEMA_VERSION, METRIC_SELF_PROCESS_CPU_PERCENT, METRIC_SELF_PROCESS_HANDLES,
    METRIC_SELF_PROCESS_THREADS, METRIC_SELF_PROCESS_WORKING_SET_MB,
};

/// 单拍自体采样值（cpu_percent 为 None = 首拍无基线，不出 CPU 行）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SelfSample {
    pub cpu_percent: Option<f64>,
    pub working_set_mb: f64,
    pub threads: u32,
    pub handles: u32,
}

/// 差分计算（纯函数，单测钉住口径）：全核累计 100ns → 归一 0-100%。
pub fn cpu_percent_between(
    prev_cpu_time_100ns: u64,
    prev_wall_ms: u64,
    cur_cpu_time_100ns: u64,
    cur_wall_ms: u64,
    logical_cores: f64,
) -> Option<f64> {
    let wall = cur_wall_ms.saturating_sub(prev_wall_ms) as f64;
    if wall <= 0.0 || logical_cores <= 0.0 {
        return None;
    }
    let cores = if logical_cores < 1.0 {
        1.0
    } else {
        logical_cores
    };
    let delta_s = cur_cpu_time_100ns.saturating_sub(prev_cpu_time_100ns) as f64 / 10_000_000.0;
    Some(((delta_s / (wall / 1000.0)) * 100.0 / cores).clamp(0.0, 100.0))
}

/// 采样自身进程（Windows API 直读；单测不触此函数，纯函数部分单测钉口径）。
pub fn read_self_sample(
    prev_cpu_time_100ns: u64,
    prev_wall_ms: u64,
) -> (u64, u64, u64, SelfSample) {
    use windows_sys::Win32::Foundation::{CloseHandle, FILETIME};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD,
    };
    use windows_sys::Win32::System::ProcessStatus::{
        GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
    };
    use windows_sys::Win32::System::Threading::{
        GetProcessHandleCount, GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    unsafe {
        let pid = std::process::id();
        let mut cpu_now = prev_cpu_time_100ns;
        let mut working_set_mb = 0.0f64;
        let mut handles = 0u32;
        let mut exe_start_epoch: u64 = 0;
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if !handle.is_null() {
            let mut creation: FILETIME = std::mem::zeroed();
            let mut exit: FILETIME = std::mem::zeroed();
            let mut kernel: FILETIME = std::mem::zeroed();
            let mut user: FILETIME = std::mem::zeroed();
            if GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user) != 0 {
                let to_u64 =
                    |ft: FILETIME| (ft.dwHighDateTime as u64) << 32 | ft.dwLowDateTime as u64;
                cpu_now = to_u64(kernel).saturating_add(to_u64(user));
                // 进程启动时刻（FILETIME 100ns → Unix epoch 秒；M2.3 进程身份防 PID 复用）
                exe_start_epoch = (to_u64(creation) / 10_000_000).saturating_sub(11_644_473_600);
            }
            let mut pmc: PROCESS_MEMORY_COUNTERS = std::mem::zeroed();
            pmc.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
            if GetProcessMemoryInfo(handle, &mut pmc, pmc.cb) != 0 {
                working_set_mb = pmc.WorkingSetSize as f64 / (1024.0 * 1024.0);
            }
            GetProcessHandleCount(handle, &mut handles);
            CloseHandle(handle);
        }
        // 线程数：线程快照过滤归属 pid（快照成本低，30s 一拍可接受）。
        let mut threads = 0u32;
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
        if !snap.is_null() {
            let mut entry: windows_sys::Win32::System::Diagnostics::ToolHelp::THREADENTRY32 =
                std::mem::zeroed();
            entry.dwSize = std::mem::size_of::<
                windows_sys::Win32::System::Diagnostics::ToolHelp::THREADENTRY32,
            >() as u32;
            if windows_sys::Win32::System::Diagnostics::ToolHelp::Thread32First(snap, &mut entry)
                != 0
            {
                loop {
                    if entry.th32OwnerProcessID == pid {
                        threads += 1;
                    }
                    if windows_sys::Win32::System::Diagnostics::ToolHelp::Thread32Next(
                        snap, &mut entry,
                    ) == 0
                    {
                        break;
                    }
                }
            }
            CloseHandle(snap);
        }
        let wall_now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let cpu_percent = cpu_percent_between(
            prev_cpu_time_100ns,
            prev_wall_ms,
            cpu_now,
            wall_now,
            std::thread::available_parallelism()
                .map(|n| n.get() as f64)
                .unwrap_or(1.0),
        );
        (
            cpu_now,
            wall_now,
            exe_start_epoch,
            SelfSample {
                cpu_percent,
                working_set_mb,
                threads,
                handles,
            },
        )
    }
}

/// 采样值 → MetricPoint 信封出站（source=WinApi，scope=本进程，quality 按
/// CPU 基线有无分：有基线 Confirmed，首拍 CPU 不出行）。
pub fn build_points(
    now_ms: i64,
    monotonic_ms: u64,
    interval_ms: u64,
    host_id: &str,
    exe_start_epoch: u64,
    s: &SelfSample,
) -> Vec<MetricPoint> {
    let pid = std::process::id();
    let mut pts = Vec::with_capacity(4);
    if let Some(cpu) = s.cpu_percent {
        pts.push(point(
            METRIC_SELF_PROCESS_CPU_PERCENT,
            now_ms,
            monotonic_ms,
            interval_ms,
            host_id,
            pid,
            exe_start_epoch,
            cpu,
            MetricUnit::Percent,
        ));
    }
    pts.push(point(
        METRIC_SELF_PROCESS_WORKING_SET_MB,
        now_ms,
        monotonic_ms,
        interval_ms,
        host_id,
        pid,
        exe_start_epoch,
        s.working_set_mb,
        MetricUnit::Mib,
    ));
    pts.push(point(
        METRIC_SELF_PROCESS_THREADS,
        now_ms,
        monotonic_ms,
        interval_ms,
        host_id,
        pid,
        exe_start_epoch,
        s.threads as f64,
        MetricUnit::Count,
    ));
    pts.push(point(
        METRIC_SELF_PROCESS_HANDLES,
        now_ms,
        monotonic_ms,
        interval_ms,
        host_id,
        pid,
        exe_start_epoch,
        s.handles as f64,
        MetricUnit::Count,
    ));
    pts
}

#[allow(clippy::too_many_arguments)]
fn point(
    metric_id: &str,
    now_ms: i64,
    monotonic_ms: u64,
    interval_ms: u64,
    host_id: &str,
    pid: u32,
    exe_start_epoch: u64,
    value: f64,
    unit: MetricUnit,
) -> MetricPoint {
    MetricPoint {
        metric_id: metric_id.to_string(),
        source: MetricSource::WinApi,
        host_id: host_id.to_string(),
        process_identity: ProcessIdentity::Id {
            pid,
            exe_start_epoch,
        },
        timestamp_utc_ms: now_ms,
        monotonic_ms,
        interval_ms,
        value,
        unit,
        scope: MetricScope::Process(ProcessIdentity::Id {
            pid,
            exe_start_epoch,
        }),
        quality: MetricQuality::Confirmed,
        loss_count: None,
        coverage: None,
        schema_version: METRIC_SCHEMA_VERSION,
    }
}

/// 进程内单调时钟（MetricPoint.monotonic_ms：相对首次调用的毫秒）。
fn monotonic_ms() -> u64 {
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    START
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_millis() as u64
}

fn host_id() -> String {
    std::env::var("COMPUTERNAME").unwrap_or_else(|_| "unknown".into())
}

/// SelfMetrics 线程状态（线程私有）。
#[derive(Debug, Default)]
pub struct TickState {
    pub cpu_time: u64,
    pub wall_ms: u64,
    pub last_bucket: i64,
    pub exe_start_epoch: u64,
}

/// 一轮采样（调用方控制节奏，建议 30-60s）：分钟桶滚动时出站落库，返回本轮写入行数。
/// 采样/写入失败只 log，不阻塞采集（诊断数据）。
pub fn tick(state: &mut TickState, db: &crate::db::Database) -> usize {
    let (cpu_time, wall_ms, exe_start, sample) = read_self_sample(state.cpu_time, state.wall_ms);
    state.cpu_time = cpu_time;
    state.wall_ms = wall_ms;
    state.exe_start_epoch = if exe_start == 0 {
        state.exe_start_epoch
    } else {
        exe_start
    };
    let now_ms = wall_ms as i64;
    let bucket = now_ms.div_euclid(60_000);
    if bucket == state.last_bucket {
        return 0;
    }
    state.last_bucket = bucket;
    let pts = build_points(
        now_ms,
        monotonic_ms(),
        60_000,
        &host_id(),
        state.exe_start_epoch,
        &sample,
    );
    let n = pts.len();
    db.insert_self_metric_points(&pts);
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_percent_diff_normalizes_by_cores_and_clamps() {
        // 1 核机器：500ms 墙钟内用满 500ms CPU = 100%
        let full = cpu_percent_between(0, 0, 5_000_000, 500, 1.0).unwrap();
        assert!((full - 100.0).abs() < 0.001);
        // 8 核机器：同样 500ms CPU 只占整机 12.5%
        let eighth = cpu_percent_between(0, 0, 5_000_000, 500, 8.0).unwrap();
        assert!((eighth - 12.5).abs() < 0.001);
        // 回退：CPU 时间倒退（不可能但防御）钳 0
        assert_eq!(
            cpu_percent_between(9_000_000, 0, 5_000_000, 500, 1.0),
            Some(0.0)
        );
        // 墙钟不前进：None（不出值，不出 estimated）
        assert_eq!(cpu_percent_between(0, 100, 5_000_000, 100, 1.0), None);
    }

    #[test]
    fn build_points_carries_full_envelope_and_skips_cpu_on_first_sample() {
        let s = SelfSample {
            cpu_percent: None, // 首拍
            working_set_mb: 40.5,
            threads: 12,
            handles: 345,
        };
        let pts = build_points(1_749_000_000_000, 42, 30_000, "HOST-1", 777, &s);
        assert_eq!(pts.len(), 3, "首拍无 CPU 基线：不出 CPU 行");
        assert!(pts.iter().all(|p| p.metric_id.starts_with("self.process.")));
        let ws = pts
            .iter()
            .find(|p| p.metric_id == METRIC_SELF_PROCESS_WORKING_SET_MB)
            .unwrap();
        assert!((ws.value - 40.5).abs() < 1e-9);
        assert!(matches!(ws.unit, MetricUnit::Mib));
        assert!(matches!(ws.source, MetricSource::WinApi));
        assert_eq!(ws.schema_version, METRIC_SCHEMA_VERSION);
        assert_eq!(ws.interval_ms, 30_000);
        match &ws.process_identity {
            ProcessIdentity::Id {
                pid,
                exe_start_epoch,
            } => {
                assert_eq!(*pid, std::process::id());
                assert_eq!(*exe_start_epoch, 777);
            }
            _ => panic!("自体指标必须带进程身份"),
        }
        // 有基线：4 行全出，CPU 归一口径
        let s2 = SelfSample {
            cpu_percent: Some(12.5),
            ..s
        };
        let pts2 = build_points(1_749_000_000_000, 42, 30_000, "HOST-1", 777, &s2);
        assert_eq!(pts2.len(), 4);
        let cpu = pts2
            .iter()
            .find(|p| p.metric_id == METRIC_SELF_PROCESS_CPU_PERCENT)
            .unwrap();
        assert!((cpu.value - 12.5).abs() < 1e-9);
        assert!(matches!(cpu.quality, MetricQuality::Confirmed));
        assert_eq!(cpu.host_id, "HOST-1");
    }
}
