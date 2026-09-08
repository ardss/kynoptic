//! 进程快照监控

use crate::types::*;
use serde_json::json;
use std::cell::Cell;
use std::collections::HashMap;
use std::mem::{size_of, zeroed};
use std::time::Duration;
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::System::Diagnostics::ToolHelp::*;
use windows_sys::Win32::System::ProcessStatus::*;
use windows_sys::Win32::System::Threading::*;

/// 系统进程名（小写），采集时跳过
const SKIP_PROCESSES: &[&str] = &[
    "system",
    "registry",
    "smss.exe",
    "csrss.exe",
    "wininit.exe",
    "services.exe",
    "lsass.exe",
    "svchost.exe",
    "fontdrvhost.exe",
    "dwm.exe",
    "conhost.exe",
    "sihost.exe",
    "taskhostw.exe",
    "explorer.exe",
    "dllhost.exe",
    "ctfmon.exe",
    "searchindexer.exe",
    "runtimebroker.exe",
    "securityhealthsystray.exe",
    "shellexperiencehost.exe",
];

pub struct ProcessMonitor {
    prev_top_key: Cell<Option<String>>,
}

impl Default for ProcessMonitor {
    fn default() -> Self {
        Self {
            prev_top_key: Cell::new(None),
        }
    }
}

impl Monitor for ProcessMonitor {
    fn name(&self) -> &str {
        "process"
    }
    fn interval(&self) -> Duration {
        Duration::from_secs(30)
    }

    fn collect(&self, tx: &crossbeam_channel::Sender<Event>) {
        let snapshot = collect_processes();

        // 用 top 进程名拼接作为变化指纹
        let key: String = snapshot
            .top_cpu
            .iter()
            .map(|p| format!("{}:{:.0}", p.name, p.cpu_percent))
            .chain(
                snapshot
                    .top_mem
                    .iter()
                    .map(|p| format!("{}:{:.0}", p.name, p.memory_mb)),
            )
            .collect();

        let prev_key = self.prev_top_key.take();
        if prev_key.as_ref() == Some(&key) {
            self.prev_top_key.set(prev_key);
            return;
        }
        self.prev_top_key.set(Some(key));

        // 直接序列化 ProcessSnapshot struct —— 字段名（top_cpu/top_mem/total_count）
        // 与 struct 定义单一来源，不再手写 json! 宏。前端按此键名读取。
        let data = serde_json::to_value(&snapshot).unwrap_or_else(|_| json!({}));
        let event = Event::new(EventAction::ProcessSnapshot, EventType::System).data(data);

        let _ = tx.try_send(event);
    }
}

#[derive(Clone, serde::Serialize)]
struct ProcessInfo {
    pid: u32,
    name: String,
    cpu_percent: f64,
    memory_mb: f64,
}

/// process_snapshot 的 event_data 结构（强类型，字段名即契约）。
///
/// collect 直接 to_value 此结构，而非手写 json! 宏——保证 event_data 的键名
/// （top_cpu / top_mem / total_count）与 struct 字段单一来源，杜绝手写错配。
/// 前端 ProcessView / api.ts 应以此字段名为准。
#[derive(serde::Serialize)]
struct ProcessSnapshot {
    top_cpu: Vec<ProcessInfo>,
    top_mem: Vec<ProcessInfo>,
    total_count: usize,
}

fn collect_processes() -> ProcessSnapshot {
    // CPU 百分比：原生 GetProcessTimes 差分（占满 1 核 = 100），内部已含
    // 指数移动平均（EMA α=0.3）平滑，见 query_cpu_percent_map 的 v0.1 注。
    let cpu_map = query_cpu_percent_map();

    let mut all: Vec<ProcessInfo> = Vec::new();

    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return ProcessSnapshot {
                top_cpu: vec![],
                top_mem: vec![],
                total_count: 0,
            };
        }

        let mut entry: PROCESSENTRY32W = zeroed();
        entry.dwSize = size_of::<PROCESSENTRY32W>() as u32;

        if Process32FirstW(snapshot, &mut entry) != 0 {
            loop {
                let name = String::from_utf16_lossy(
                    &entry.szExeFile[..entry
                        .szExeFile
                        .iter()
                        .position(|&c| c == 0)
                        .unwrap_or(entry.szExeFile.len())],
                );

                if !should_skip(&name) {
                    let pid = entry.th32ProcessID;
                    let mem_mb = query_process_memory_mb(pid);
                    // CPU 从 WMI 批量映射取（按 pid），取不到则为 0
                    let cpu = cpu_map.get(&pid).copied().unwrap_or(0.0);
                    all.push(ProcessInfo {
                        pid,
                        name: name.clone(),
                        cpu_percent: cpu,
                        memory_mb: mem_mb,
                    });
                }

                if Process32NextW(snapshot, &mut entry) == 0 {
                    break;
                }
            }
        }

        CloseHandle(snapshot);
    }

    // Top 10 by CPU
    all.sort_by(|a, b| {
        b.cpu_percent
            .partial_cmp(&a.cpu_percent)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let top_cpu: Vec<ProcessInfo> = all.iter().take(10).cloned().collect();

    // Top 10 by memory
    all.sort_by(|a, b| {
        b.memory_mb
            .partial_cmp(&a.memory_mb)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let top_mem: Vec<ProcessInfo> = all.iter().take(10).cloned().collect();

    let total_count = all.len();
    ProcessSnapshot {
        top_cpu,
        top_mem,
        total_count,
    }
}

fn should_skip(name: &str) -> bool {
    let lower = name.to_lowercase();
    SKIP_PROCESSES.iter().any(|s| lower == *s)
}

/// 查询单个进程的内存 (MB)。CPU 不再逐个查（改用批量 WMI）。
fn query_process_memory_mb(pid: u32) -> f64 {
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return 0.0;
        }
        let mut pmc: PROCESS_MEMORY_COUNTERS = zeroed();
        pmc.cb = size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
        let mem_ok = GetProcessMemoryInfo(handle, &mut pmc, pmc.cb) != 0;
        CloseHandle(handle);
        if mem_ok {
            pmc.WorkingSetSize as f64 / (1024.0 * 1024.0)
        } else {
            0.0
        }
    }
}

/// 批量查询所有进程的 CPU 百分比，返回 PID → CPU% 映射（占满 1 核 = 100）。
///
/// v0.1 注：上游实现通过 PowerShell/WMI（Win32_PerfFormattedData_PerfProc_Process）
/// 批量读取。开源版硬性要求零子进程（见 R8-v01采集裁剪.md），改用原生
/// GetProcessTimes 差分：两次采样间的 (kernel+user) 时间增量除以墙钟时间增量。
/// EMA 平滑沿用上游的 α=0.3 口径，抑制瞬时抖动；已退出进程从平滑表淘汰。
fn query_cpu_percent_map() -> HashMap<u32, f64> {
    use std::cell::RefCell;
    use std::time::Instant;

    // pid -> 最近一次采样的 CPU 累计时间（100ns 单位，kernel+user）
    thread_local! {
        // HashMap::new 尚非 const fn（RandomState），无法用 const 初始化块
        #[allow(clippy::missing_const_for_thread_local)]
        static PREV_TIMES: RefCell<HashMap<u32, u64>> = RefCell::new(HashMap::new());
        static PREV_INST: RefCell<Option<Instant>> = const { RefCell::new(None) };
        #[allow(clippy::missing_const_for_thread_local)]
        static SMOOTHED: RefCell<HashMap<u32, f64>> = RefCell::new(HashMap::new());
    }

    let now = Instant::now();
    let mut times: HashMap<u32, u64> = HashMap::new();
    unsafe {
        // 复用进程快照遍历取 PID 集合（避免重复 CreateToolhelp32Snapshot 也无妨，
        // 这里独立快照：collect_processes 与 CPU 采样的生命周期解耦）。
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot != INVALID_HANDLE_VALUE {
            let mut entry: PROCESSENTRY32W = zeroed();
            entry.dwSize = size_of::<PROCESSENTRY32W>() as u32;
            if Process32FirstW(snapshot, &mut entry) != 0 {
                loop {
                    times.insert(entry.th32ProcessID, 0);
                    if Process32NextW(snapshot, &mut entry) == 0 {
                        break;
                    }
                }
            }
            CloseHandle(snapshot);
        }
        for (pid, total) in times.iter_mut() {
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, *pid);
            if handle.is_null() {
                continue;
            }
            let mut creation: FILETIME = zeroed();
            let mut exit: FILETIME = zeroed();
            let mut kernel: FILETIME = zeroed();
            let mut user: FILETIME = zeroed();
            if GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user) != 0 {
                let to_u64 =
                    |ft: FILETIME| (ft.dwHighDateTime as u64) << 32 | ft.dwLowDateTime as u64;
                *total = to_u64(kernel).saturating_add(to_u64(user));
            }
            CloseHandle(handle);
        }
    }

    PREV_TIMES.with(|prev| {
        PREV_INST.with(|prev_inst| {
            SMOOTHED.with(|smoothed| {
                let prev_times = prev.borrow_mut();
                let elapsed = prev_inst
                    .borrow_mut()
                    .replace(now)
                    .map(|t| now.duration_since(t).as_secs_f64())
                    .unwrap_or(0.0);
                let mut out: HashMap<u32, f64> = HashMap::new();
                const ALPHA: f64 = 0.3;
                for (pid, total) in &times {
                    let delta_pct = match (elapsed, prev_times.get(pid)) {
                        (e, Some(prev_t)) if e > 0.0 => {
                            ((total.saturating_sub(*prev_t) as f64 / 10_000_000.0) / e * 100.0)
                                .clamp(0.0, 100.0)
                        }
                        _ => 0.0,
                    };
                    let ema = match smoothed.borrow().get(pid) {
                        Some(p) => p * (1.0 - ALPHA) + delta_pct * ALPHA,
                        None => delta_pct,
                    };
                    smoothed.borrow_mut().insert(*pid, ema);
                    out.insert(*pid, ema);
                }
                // 淘汰已退出进程
                smoothed
                    .borrow_mut()
                    .retain(|pid, _| times.contains_key(pid));
                out
            })
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 契约测试：ProcessSnapshot 序列化后的 JSON 键名必须与前端 ProcessView/api.ts 读取的一致。
    /// 这是 process 数据的「字段名单一来源」——改 struct 字段名会同时改 JSON，
    /// 本测试锁定前端期望的键名，防止重命名导致前端读不到数据。
    #[test]
    fn process_snapshot_contract_keys() {
        let snap = ProcessSnapshot {
            top_cpu: vec![ProcessInfo {
                pid: 1,
                name: "test.exe".into(),
                cpu_percent: 50.0,
                memory_mb: 100.0,
            }],
            top_mem: vec![],
            total_count: 1,
        };
        let v = serde_json::to_value(&snap).unwrap();
        let obj = v.as_object().expect("snapshot is object");

        // 顶层键：前端 ProcessData.current 期望 top_cpu / top_mem / total_count
        assert!(obj.contains_key("top_cpu"), "缺 top_cpu");
        assert!(obj.contains_key("top_mem"), "缺 top_mem");
        assert!(obj.contains_key("total_count"), "缺 total_count");

        // 元素键：前端 ProcessInfo 期望 pid / name / cpu_percent / memory_mb
        let item = obj["top_cpu"].as_array().unwrap()[0].as_object().unwrap();
        assert!(item.contains_key("pid"), "缺 pid");
        assert!(item.contains_key("name"), "缺 name");
        assert!(
            item.contains_key("cpu_percent"),
            "缺 cpu_percent（前端读这个）"
        );
        assert!(item.contains_key("memory_mb"), "缺 memory_mb（前端读这个）");
    }
}
