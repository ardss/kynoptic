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
    prev_fingerprint: Cell<Option<String>>,
    /// 上次实际落库时间（低频心跳用：即使指纹未变也周期性强制写一行，
    /// 证明采集线程还活着）
    last_emitted: Cell<Option<std::time::Instant>>,
}

impl Default for ProcessMonitor {
    fn default() -> Self {
        Self {
            prev_fingerprint: Cell::new(None),
            last_emitted: Cell::new(None),
        }
    }
}

/// CPU% 容差：EMA 平滑值几乎每个采样点都微变，±1.5% 内视为同一桶（写放大
/// 修复：旧实现每 30s 全量落库，event_data 约 2-4KB，主项 ~8MB/天）。
const CPU_TOLERANCE_PCT: f64 = 1.5;
/// 心跳强制落库间隔：指纹未变也每 10 分钟写一行，证明监控器存活。
const HEARTBEAT_SECS: u64 = 600;

/// CPU% 分桶：按容差 1.5% 取整桶号。
fn cpu_bucket(v: f64) -> i32 {
    (v / CPU_TOLERANCE_PCT).round() as i32
}

/// 快照变化指纹（纯函数，可测）：进程数 + top 条目的 pid/CPU 桶/整数 MB 内存。
/// 任一超出容差的变化都会改变指纹；EMA 微抖动（<1.5%）不改变。
fn snapshot_fingerprint(s: &ProcessSnapshot) -> String {
    let mut parts: Vec<String> = vec![format!("n={}", s.total_count)];
    for p in s.top_cpu.iter().chain(s.top_mem.iter()) {
        parts.push(format!(
            "{}:{}:{}",
            p.pid,
            cpu_bucket(p.cpu_percent),
            p.memory_mb.round() as i64
        ));
    }
    parts.join("|")
}

impl Monitor for ProcessMonitor {
    fn name(&self) -> &str {
        "process"
    }
    fn interval(&self) -> Duration {
        // 30s→120s（性能审查）：每 tick 两次全进程快照 + 逐 pid
        // OpenProcess/GetProcessTimes/GetProcessMemoryInfo 的句柄开销使本
        // monitor 单开 avg CPU 2~3.2%、每 30s 打出 >10% 单核尖峰（实测
        // 128 样本），占默认集稳态 CPU 约 7 成。拉长 tick 直接按比例摊薄；
        // 数据新鲜度由 HEARTBEAT_SECS 心跳与指纹去重语义兜底，契约不变。
        Duration::from_secs(120)
    }

    fn collect(&self, tx: &crossbeam_channel::Sender<Event>) {
        let Some(snapshot) = collect_processes() else {
            return; // 快照失败：本轮无数据可写（ fingerprint/last_emitted 均不动 ）
        };
        let key = snapshot_fingerprint(&snapshot);

        let unchanged = self
            .prev_fingerprint
            .take()
            .as_ref()
            .map(|prev| prev == &key)
            .unwrap_or(false);
        let heartbeat_due = self
            .last_emitted
            .get()
            .map(|t| t.elapsed() >= Duration::from_secs(HEARTBEAT_SECS))
            .unwrap_or(true);

        if unchanged && !heartbeat_due {
            self.prev_fingerprint.set(Some(key));
            return;
        }
        self.prev_fingerprint.set(Some(key));
        self.last_emitted.set(Some(std::time::Instant::now()));

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

fn collect_processes() -> Option<ProcessSnapshot> {
    // CPU 百分比：原生 GetProcessTimes 差分（占满 1 核 = 100），内部已含
    // 指数移动平均（EMA α=0.3）平滑，见 query_cpu_percent_map 的 v0.1 注。
    //
    // 快照合并（性能审查）：旧实现 CPU 采样内部再开一次全进程快照，每 tick
    // 两次 CreateToolhelp32Snapshot；现在只开一次，遍历中顺手收集 pid 集合
    // 传给差分采样。

    let mut all: Vec<ProcessInfo> = Vec::new();
    let mut pids: Vec<u32> = Vec::new();

    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            // Wave19 P0：快照失败必须让调用方跳过本轮——返回假"空系统"
            // 快照会与真实指纹必然不同，落一条"进程全灭"的污染事件
            return None;
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
                pids.push(entry.th32ProcessID);

                if !should_skip(&name) {
                    let pid = entry.th32ProcessID;
                    let mem_mb = query_process_memory_mb(pid);
                    // CPU 稍后从批量差分映射统一回填（见函数头注释）
                    all.push(ProcessInfo {
                        pid,
                        name: name.clone(),
                        cpu_percent: 0.0,
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

    // 快照遍历完成后再做逐 pid GetProcessTimes 差分（EMA 状态跨 tick 保持）
    let cpu_map = query_cpu_percent_map(&pids);
    for p in all.iter_mut() {
        p.cpu_percent = cpu_map.get(&p.pid).copied().unwrap_or(0.0);
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
    Some(ProcessSnapshot {
        top_cpu,
        top_mem,
        total_count,
    })
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
/// pid 集合由调用方从进程快照遍历中顺手收集后传入（性能审查：旧实现内部
/// 再开一次全进程快照，每 tick 两次 CreateToolhelp32Snapshot）。
///
/// v0.1 注：上游实现通过 PowerShell/WMI（Win32_PerfFormattedData_PerfProc_Process）
/// 批量读取。开源版硬性要求零子进程（见 R8-v01采集裁剪.md），改用原生
/// GetProcessTimes 差分：两次采样间的 (kernel+user) 时间增量除以墙钟时间增量。
/// EMA 平滑沿用上游的 α=0.3 口径，抑制瞬时抖动；已退出进程从平滑表淘汰。
fn query_cpu_percent_map(pids: &[u32]) -> HashMap<u32, f64> {
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
    let mut times: HashMap<u32, u64> = pids.iter().map(|&pid| (pid, 0)).collect();
    unsafe {
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
                let mut prev_times = prev.borrow_mut();
                let elapsed = prev_inst
                    .borrow_mut()
                    .replace(now)
                    .map(|t| now.duration_since(t).as_secs_f64())
                    .unwrap_or(0.0);
                let mut out: HashMap<u32, f64> = HashMap::new();
                const ALPHA: f64 = 0.3;
                // 全库审查 P1：PREV_TIMES 此前只读不写——CPU% 恒为 0。
                // 先取上一轮快照，本轮结束回写，供下一轮差分。
                let prev_snapshot: HashMap<u32, u64> = prev_times.clone();
                for (pid, total) in &times {
                    prev_times.insert(*pid, *total);
                }
                for (pid, total) in &times {
                    let delta_pct = match (elapsed, prev_snapshot.get(pid)) {
                        (e, Some(prev_t)) if e > 0.0 => {
                            // GetProcessTimes 是全核累计：8 核满载 = 800%。
                            // 除以逻辑核数归一成"占整机百分比"（Wave19 P0：
                            // 原来钳到 100 让多核进程排名失真）
                            let cores = std::thread::available_parallelism()
                                .map(|n| n.get() as f64)
                                .unwrap_or(1.0);
                            (((total.saturating_sub(*prev_t) as f64 / 10_000_000.0) / e * 100.0)
                                / cores)
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
                // 淘汰已退出进程（Wave19 P1：prev_times 同步清理，否则
                // 进程 churn 会无限残留条目）
                smoothed
                    .borrow_mut()
                    .retain(|pid, _| times.contains_key(pid));
                prev_times.retain(|pid, _| times.contains_key(pid));
                out
            })
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_snapshot_fingerprint_tolerance() {
        let mk = |cpu: f64, mem: f64, total: usize| ProcessSnapshot {
            top_cpu: vec![ProcessInfo {
                pid: 1,
                name: "a.exe".into(),
                cpu_percent: cpu,
                memory_mb: mem,
            }],
            top_mem: vec![],
            total_count: total,
        };
        // EMA 微抖动（<1.5% 容差 + 内存 <0.5MB 抖动）→ 指纹不变 → 跳过落库
        assert_eq!(
            snapshot_fingerprint(&mk(10.0, 100.0, 200)),
            snapshot_fingerprint(&mk(10.9, 100.3, 200))
        );
        // CPU 变化超容差 / 内存超 1MB / 进程数变化 → 指纹变化 → 落库
        assert_ne!(
            snapshot_fingerprint(&mk(10.0, 100.0, 200)),
            snapshot_fingerprint(&mk(12.5, 100.0, 200))
        );
        assert_ne!(
            snapshot_fingerprint(&mk(10.0, 100.0, 200)),
            snapshot_fingerprint(&mk(10.0, 102.0, 200))
        );
        assert_ne!(
            snapshot_fingerprint(&mk(10.0, 100.0, 200)),
            snapshot_fingerprint(&mk(10.0, 100.0, 201))
        );
    }

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
