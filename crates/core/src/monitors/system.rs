//! 系统资源监控（内存 + CPU）

use crate::types::*;
use serde_json::json;
use std::cell::Cell;
use std::mem::{size_of, zeroed};
use std::time::Duration;
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::System::SystemInformation::*;
use windows_sys::Win32::System::Threading::GetSystemTimes;

pub struct SystemMonitor;

impl Default for SystemMonitor {
    fn default() -> Self {
        Self
    }
}

impl Monitor for SystemMonitor {
    fn name(&self) -> &str {
        "system"
    }

    fn interval(&self) -> Duration {
        Duration::from_secs(60)
    }

    fn collect(&self, tx: &crossbeam_channel::Sender<Event>) {
        let mem_info = collect_memory();
        let cpu_usage = collect_cpu();

        let event = Event::new(EventAction::Heartbeat, EventType::System).data(json!({
            "memory": mem_info,
            "cpu_percent": cpu_usage,
        }));

        let _ = tx.try_send(event);
    }
}

fn collect_memory() -> serde_json::Value {
    unsafe {
        let mut status: MEMORYSTATUSEX = zeroed();
        status.dwLength = size_of::<MEMORYSTATUSEX>() as u32;
        GlobalMemoryStatusEx(&mut status);

        json!({
            "total": status.ullTotalPhys,
            "available": status.ullAvailPhys,
            "used_percent": status.dwMemoryLoad,
        })
    }
}

/// 通过 GetSystemTimes 计算 CPU 使用率
/// 返回瞬时快照，首次调用返回 0.0
struct CpuState {
    idle: Cell<u64>,
    kernel: Cell<u64>,
    user: Cell<u64>,
}

thread_local! {
    static CPU_STATE: CpuState = const { CpuState {
        idle: Cell::new(0),
        kernel: Cell::new(0),
        user: Cell::new(0),
    } };
}

fn collect_cpu() -> f64 {
    CPU_STATE.with(|state| unsafe {
        let mut idle: FILETIME = zeroed();
        let mut kernel: FILETIME = zeroed();
        let mut user: FILETIME = zeroed();

        if GetSystemTimes(&mut idle, &mut kernel, &mut user) == 0 {
            return 0.0;
        }

        let idle_now = ft_to_u64(&idle);
        let kernel_now = ft_to_u64(&kernel);
        let user_now = ft_to_u64(&user);

        let idle_prev = state.idle.get();
        let kernel_prev = state.kernel.get();
        let user_prev = state.user.get();

        state.idle.set(idle_now);
        state.kernel.set(kernel_now);
        state.user.set(user_now);

        let idle_delta = idle_now.saturating_sub(idle_prev);
        let kernel_delta = kernel_now.saturating_sub(kernel_prev);
        let user_delta = user_now.saturating_sub(user_prev);

        let total_delta = kernel_delta + user_delta;
        if total_delta == 0 {
            return 0.0;
        }

        let busy_delta = total_delta.saturating_sub(idle_delta);
        (busy_delta as f64 / total_delta as f64) * 100.0
    })
}

fn ft_to_u64(ft: &FILETIME) -> u64 {
    ((ft.dwHighDateTime as u64) << 32) | (ft.dwLowDateTime as u64)
}
