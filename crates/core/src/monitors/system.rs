//! 系统资源监控（内存 + CPU）
//!
//! 写门控（发现 platform low：旧实现每 tick 无条件落 1 条 heartbeat，
//! 空闲地板 360 条/h 事件流）：改「变化才落 + 600s 强制心跳」，与 process
//! 监控器同构（快照指纹去重）。空闲地板 360/h → 约 6/h；数据键名契约不变。
//! 600s 心跳的真实约束在 mcp 消费侧（System/Heartbeat 事件）：network_down
//! 的 900s 新鲜度窗口（crates/mcp/src/state.rs，600s + 至多 30s flush 滞后
//! < 900s）。与 tray 看门狗无关：其 1800s 停滞判定读的是 writer 的
//! last_flush_epoch（每次成功落库刷新，见 crates/tray/src/main.rs），tray
//! 与 watchdog 二进制均不读 DB 里的 heartbeat 事件——把 600s 提到更大不会
//! 触发任何「误杀」，只会放宽 mcp 的新鲜度前提。

use crate::types::*;
use serde_json::json;
use std::cell::Cell;
use std::mem::{size_of, zeroed};
use std::time::Duration;
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::System::SystemInformation::*;
use windows_sys::Win32::System::Threading::GetSystemTimes;

/// 内存快照（GlobalMemoryStatusEx 原生读取；JSON 字段名为前端/洞察读取契约）
#[derive(Clone, Copy)]
struct MemoryInfo {
    total: u64,
    available: u64,
    used_percent: u32,
}

impl MemoryInfo {
    fn as_json(&self) -> serde_json::Value {
        json!({
            "total": self.total,
            "available": self.available,
            "used_percent": self.used_percent,
        })
    }
}

pub struct SystemMonitor {
    prev_fingerprint: Cell<Option<String>>,
    /// 上次实际落库时间（低频心跳用：指纹未变也周期性强制写一行，
    /// 证明采集线程还活着）
    last_emitted: Cell<Option<std::time::Instant>>,
}

impl Default for SystemMonitor {
    fn default() -> Self {
        Self {
            prev_fingerprint: Cell::new(None),
            last_emitted: Cell::new(None),
        }
    }
}

/// 心跳强制落库间隔：指纹未变也每 10 分钟写一行，证明监控器存活。
/// 约束来自 mcp 消费方而非 tray 看门狗：network_down 的新鲜度窗口 900s
/// （crates/mcp/src/state.rs）要求心跳间隔 + flush 滞后 < 900s，否则空闲期
/// 误判「系统无心跳」漏报网络不可用（state.rs 同处同口径注释）。tray 的
/// 1800s 停滞判定读 writer 的 last_flush_epoch（成功落库刷新），不读 DB
/// heartbeat 事件——本常量与它无约束关系（模块头注同款说明）。
const HEARTBEAT_SECS: u64 = 600;
/// CPU% 分桶容差：系统 CPU 是 10s 瞬时差分快照（非 EMA），空闲抖动小，
/// 1.5% 桶与 process 监控器同口径，桶内微抖动不翻转指纹。
const CPU_TOLERANCE_PCT: f64 = 1.5;

/// 系统资源变化指纹（纯函数，可测）：内存负载整数 % + CPU 分桶号。
/// 空闲微抖动（CPU <1.5% 桶、内存负载整数不变）下指纹不变 → 跳过落库；
/// 任一真实变化 → 指纹变 → 落库。available/total 原始值不进指纹
/// （available 有 KB 级正常波动，进指纹会把空闲地板重新抬回去）。
fn system_fingerprint(mem: &MemoryInfo, cpu: f64) -> String {
    format!("m{}|c{}", mem.used_percent, cpu_bucket(cpu))
}

/// CPU% 分桶：按容差 1.5% 取整桶号（与 process 监控器同口径）
fn cpu_bucket(v: f64) -> i32 {
    (v / CPU_TOLERANCE_PCT).round() as i32
}

/// 是否落库（纯函数，可测）：指纹变化、或强制心跳到期（age ≥ 600s）即写；
/// 首跳（prev=None / age=None）恒写。测试不读真实时钟——age 由调用方
/// 以显式 Duration 传入。
fn should_emit(prev: Option<&str>, age: Option<Duration>, key: &str) -> bool {
    let unchanged = prev.map(|p| p == key).unwrap_or(false);
    let heartbeat_due = age
        .map(|a| a >= Duration::from_secs(HEARTBEAT_SECS))
        .unwrap_or(true);
    !unchanged || heartbeat_due
}

impl Monitor for SystemMonitor {
    fn name(&self) -> &str {
        "system"
    }

    fn interval(&self) -> Duration {
        // 轮询节拍保持不变（低配模式 30s）：采集本身近乎零成本，
        // 写库频率由下方指纹门控决定（空闲 360/h → 约 6/h），数据口径不变
        if super::low_power_mode() {
            Duration::from_secs(30)
        } else {
            Duration::from_secs(10)
        }
    }

    fn collect(&self, tx: &crossbeam_channel::Sender<Event>) {
        let mem = collect_memory();
        let cpu_usage = collect_cpu();
        let key = system_fingerprint(&mem, cpu_usage);

        let prev = self.prev_fingerprint.take();
        let age = self.last_emitted.get().map(|t| t.elapsed());
        if !should_emit(prev.as_deref(), age, &key) {
            self.prev_fingerprint.set(Some(key));
            return;
        }
        self.prev_fingerprint.set(Some(key));
        self.last_emitted.set(Some(std::time::Instant::now()));

        // event_data 键形（memory{total,available,used_percent}/cpu_percent）与旧实现
        // 一致，mcp state / insights / 前端按此键读取，单一来源见 MemoryInfo::as_json。
        let event = Event::new(EventAction::Heartbeat, EventType::System).data(json!({
            "memory": mem.as_json(),
            "cpu_percent": cpu_usage,
        }));

        let _ = tx.try_send(event);
    }
}

fn collect_memory() -> MemoryInfo {
    unsafe {
        let mut status: MEMORYSTATUSEX = zeroed();
        status.dwLength = size_of::<MEMORYSTATUSEX>() as u32;
        GlobalMemoryStatusEx(&mut status);

        MemoryInfo {
            total: status.ullTotalPhys,
            available: status.ullAvailPhys,
            used_percent: status.dwMemoryLoad,
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn mem(used_percent: u32) -> MemoryInfo {
        MemoryInfo {
            total: 1 << 40,
            available: 1 << 38,
            used_percent,
        }
    }

    /// 空闲微抖动不翻转指纹（CPU 1.5% 桶内、内存负载整数不变）；
    /// 任一真实变化 → 指纹变 → 落库。
    #[test]
    fn system_fingerprint_gating() {
        // CPU 桶内微抖动（0.2→0.7 同桶 0）→ 指纹不变（空闲跳过落库）
        assert_eq!(
            system_fingerprint(&mem(23), 0.2),
            system_fingerprint(&mem(23), 0.7)
        );
        // CPU 超桶（0.2→1.0）→ 指纹变（真实变化落库）
        assert_ne!(
            system_fingerprint(&mem(23), 0.2),
            system_fingerprint(&mem(23), 1.0)
        );
        // 内存负载整数 % 变化 → 指纹变
        assert_ne!(
            system_fingerprint(&mem(23), 0.2),
            system_fingerprint(&mem(24), 0.2)
        );
        // available/total 原始值不进指纹（KB 级波动不触发写入）
        let m2 = MemoryInfo {
            total: (1 << 40) + 1,
            available: 0,
            used_percent: 23,
        };
        assert_eq!(
            system_fingerprint(&mem(23), 0.2),
            system_fingerprint(&m2, 0.2)
        );
    }

    /// 落库门控（纯函数，不读真实时钟）：首跳恒写；指纹未变且未到期 → 跳过；
    /// 指纹未变但 600s 强制心跳到期 → 写；指纹变化 → 写。
    #[test]
    fn should_emit_gating() {
        // 首跳：prev=None、age=None → 必写
        assert!(should_emit(None, None, "m23|c0"));
        // 指纹未变、距上次落库未满 600s → 跳过（空闲不写）
        assert!(!should_emit(
            Some("m23|c0"),
            Some(Duration::from_secs(599)),
            "m23|c0"
        ));
        // 指纹未变、满 600s 强制心跳 → 写
        assert!(should_emit(
            Some("m23|c0"),
            Some(Duration::from_secs(600)),
            "m23|c0"
        ));
        // 指纹变化（内存负载 23→24）→ 写，与 age 无关
        assert!(should_emit(
            Some("m23|c0"),
            Some(Duration::from_secs(0)),
            "m24|c0"
        ));
        // 指纹变化（CPU 桶 0→1）→ 写
        assert!(should_emit(
            Some("m23|c0"),
            Some(Duration::from_secs(0)),
            "m23|c1"
        ));
    }

    /// event_data 键形契约（纯函数，不读真实时钟）：MemoryInfo::as_json 的
    /// 键名即 mcp state / insights / 前端读取契约，单一来源在此锁定。
    #[test]
    fn memory_info_json_keys() {
        let m = MemoryInfo {
            total: 100,
            available: 40,
            used_percent: 60,
        };
        let v = m.as_json();
        assert_eq!(v["total"], 100);
        assert_eq!(v["available"], 40);
        assert_eq!(v["used_percent"], 60);
    }
}
