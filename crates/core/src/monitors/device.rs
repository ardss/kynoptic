//! 设备硬件快照（内存 + 磁盘 + 磁盘 I/O 速率）

use crate::types::*;
use serde_json::json;
use std::mem::{size_of, zeroed};
use std::time::Duration;
use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
use windows_sys::Win32::System::SystemInformation::*;

/// 设备硬件快照监控器。
///
/// `enable_disk_io` 控制磁盘 I/O 速率采集（见 [`collect_disk_io`]）：
/// 完整实现已从上游恢复，但它依赖 PowerShell/WMI 子进程，而 device 是
/// 默认启用的 14 个监控器之一（零子进程硬约束，见 CODE_NOTES.md §9），
/// 故默认 `false`（disk_io 字段省略）。需要 I/O 速率时显式启用，
/// 或等原生方案（PDH/IOCTL）落地后转默认。
#[derive(Debug, Default)]
pub struct DeviceMonitor {
    /// 是否采集磁盘 I/O 速率（PS 子进程，默认关闭）
    pub enable_disk_io: bool,
    /// 上次输入设备拓扑（Raw Input 枚举，仅变化时写行）
    last_input_topology: std::sync::Mutex<Option<Vec<crate::raw_input_devices::InputDeviceInfo>>>,
}

impl Monitor for DeviceMonitor {
    fn name(&self) -> &str {
        "device"
    }

    fn interval(&self) -> Duration {
        // 30s：磁盘容量变化慢，但 disk_io 是速率指标，前端 StorageView 需要较新数据。
        // 此前 120s 导致磁盘 I/O 卡长时间不更新。
        Duration::from_secs(30)
    }

    fn collect(&self, tx: &crossbeam_channel::Sender<Event>) {
        // 输入设备拓扑（型号能力/VID/PID）：仅与上次不同才随快照落库
        let input_devices = crate::raw_input_devices::enumerate();
        let changed =
            crate::raw_input_devices::changed_since(&input_devices, self.last_input_topology.lock().ok().as_deref().and_then(|g| g.as_deref()));
        if changed {
            if let Ok(mut g) = self.last_input_topology.lock() {
                *g = Some(input_devices.clone());
            }
        }
        let snapshot = DeviceSnapshot {
            memory: collect_memory(),
            disks: collect_disk(),
            disk_io: if self.enable_disk_io {
                collect_disk_io()
            } else {
                None
            },
            input_devices: if changed {
                Some(input_devices)
            } else {
                None
            },
        };
        // 直接序列化 struct —— 字段名（memory/disks/disk_io 及其子字段）单一来源，
        // 不再手写 json! 宏。disk_io 为 None 时 skip_serializing_if 自动省略。
        let data = serde_json::to_value(&snapshot).unwrap_or_else(|_| json!({}));
        let event = Event::new(EventAction::DeviceSnapshot, EventType::Device).data(data);

        let _ = tx.try_send(event);
    }
}

// ─── device_snapshot 的强类型契约（字段名即 event_data 键名单一来源）────
// 这些 struct 直接 serde::Serialize 进 event_data，不再手写 json! 宏，
// 保证 StorageView（disks/disk_io）和 HardwareView（memory）读到的键名与定义一致。
// 重命名任意字段 → 契约测试失败 + 前端类型需同步更新。

/// 内存状态（heartbeat.device_snapshot.memory）
#[derive(serde::Serialize)]
struct MemInfo {
    total_gb: u64,
    available_gb: u64,
    used_percent: u32,
}

/// 单个磁盘（device_snapshot.disks 元素）
#[derive(serde::Serialize)]
struct DiskItem {
    drive: String,
    total_gb: u64,
    free_gb: u64,
    used_percent: u64,
}

/// 磁盘 I/O 速率（device_snapshot.disk_io）
#[derive(serde::Serialize, Default)]
struct DiskIo {
    read_bytes_per_sec: f64,
    write_bytes_per_sec: f64,
}

/// device_snapshot 顶层结构
#[derive(serde::Serialize)]
struct DeviceSnapshot {
    memory: MemInfo,
    disks: Vec<DiskItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    disk_io: Option<DiskIo>,
    /// 输入设备拓扑快照（仅变化时随事件携带）
    #[serde(skip_serializing_if = "Option::is_none")]
    input_devices: Option<Vec<crate::raw_input_devices::InputDeviceInfo>>,
}

fn collect_memory() -> MemInfo {
    unsafe {
        let mut status: MEMORYSTATUSEX = zeroed();
        status.dwLength = size_of::<MEMORYSTATUSEX>() as u32;
        GlobalMemoryStatusEx(&mut status);

        MemInfo {
            total_gb: (status.ullTotalPhys as f64 / 1073741824.0).round() as u64,
            available_gb: (status.ullAvailPhys as f64 / 1073741824.0).round() as u64,
            used_percent: status.dwMemoryLoad,
        }
    }
}

fn collect_disk() -> Vec<DiskItem> {
    // 用 GetLogicalDriveStringsW 获取实际存在的盘符列表，
    // 替代此前对 A-Z 全部 26 个盘符逐一调用 GetDiskFreeSpaceExW（多为失败调用）。
    let mut disks = Vec::new();

    let drives = enumerate_logical_drives();
    for drive_path in drives {
        let mut free_available: u64 = 0;
        let mut total: u64 = 0;
        let mut total_free: u64 = 0;

        unsafe {
            if GetDiskFreeSpaceExW(
                drive_path.as_ptr(),
                &mut free_available,
                &mut total,
                &mut total_free,
            ) != 0
            {
                let drive_letter = drive_path[0] as u8 as char;
                disks.push(DiskItem {
                    drive: drive_letter.to_string(),
                    total_gb: (total as f64 / 1073741824.0).round() as u64,
                    free_gb: (free_available as f64 / 1073741824.0).round() as u64,
                    used_percent: if total > 0 {
                        (((total - free_available) as f64 / total as f64) * 100.0).round() as u64
                    } else {
                        0
                    },
                });
            }
        }
    }

    disks
}

/// 获取系统实际存在的逻辑盘符列表（如 ["C:\\", "D:\\"]）。
///
/// GetLogicalDriveStringsW 返回双 null 结尾的 UTF-16 字符串序列，
/// 每个盘符形如 "C:\\"。
fn enumerate_logical_drives() -> Vec<[u16; 4]> {
    extern "system" {
        fn GetLogicalDriveStringsW(len: u32, buffer: *mut u16) -> u32;
    }

    unsafe {
        let buf_len = GetLogicalDriveStringsW(0, std::ptr::null_mut());
        if buf_len == 0 {
            return Vec::new();
        }

        let mut buf = vec![0u16; buf_len as usize + 1];
        let written = GetLogicalDriveStringsW(buf.len() as u32, buf.as_mut_ptr());
        if written == 0 {
            return Vec::new();
        }

        // 解析双 null 结尾的字符串序列："C:\\\0D:\\\0\0"
        let mut drives = Vec::new();
        let mut start = 0;
        for i in 0..written as usize {
            if buf[i] == 0 {
                if i == start {
                    break; // 连续 null，序列结束
                }
                let s = &buf[start..i];
                // 盘符路径形如 "C:\"，取前 3 个字符 + null 终止
                if s.len() >= 3 {
                    let mut path = [0u16; 4];
                    path[..3].copy_from_slice(&s[..3]);
                    drives.push(path);
                }
                start = i + 1;
            }
        }
        drives
    }
}

/// 采集磁盘 I/O 速率（read/write bytes per sec）。
///
/// v0.2 恢复说明：上游原实现（PowerShell/WMI
/// `Win32_PerfFormattedData_PerfDisk_PhysicalDisk` 读取 `_Total` 实例速率）
/// 已原样恢复，但受"默认启用监控器零子进程"约束，仅当
/// [`DeviceMonitor::enable_disk_io`] 为 true 时调用（见结构体文档）。
/// 该 WMI 类返回**已计算好的速率值**（perf 引擎内部完成），属性名
/// `DiskReadBytesPerSec` / `DiskWriteBytesPerSec` 是 WMI API 标识符，
/// **不随系统语言变化**。此前用 `Get-Counter '\PhysicalDisk(_Total)\Disk
/// Read Bytes/sec'`，计数器路径在纯中文 Windows 上会被本地化导致静默失败。
fn collect_disk_io() -> Option<DiskIo> {
    let script = r#"
$d = Get-CimInstance -ClassName Win32_PerfFormattedData_PerfDisk_PhysicalDisk -Filter "Name='_Total'" -ErrorAction SilentlyContinue
if ($d) { "$($d.DiskReadBytesPerSec)|$($d.DiskWriteBytesPerSec)" }
"#;
    crate::monitors::ps::run_ps(script).and_then(|out| parse_disk_io(&out))
}

/// 纯函数：解析磁盘 I/O 速率字符串（"read|write"）——见 collect_disk_io。
///
/// 返回 DiskIo struct（字段名即契约）。便于单元测试：正常值、空输出、非数字、单字段。
fn parse_disk_io(out: &str) -> Option<DiskIo> {
    let parts: Vec<&str> = out.trim().split('|').collect();
    if parts.len() == 2 {
        Some(DiskIo {
            read_bytes_per_sec: parts[0].trim().parse().unwrap_or(0.0),
            write_bytes_per_sec: parts[1].trim().parse().unwrap_or(0.0),
        })
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_disk_io_normal() {
        let io = parse_disk_io("1629530|468051\n").expect("valid input");
        assert_eq!(io.read_bytes_per_sec, 1629530.0);
        assert_eq!(io.write_bytes_per_sec, 468051.0);
    }

    #[test]
    fn parse_disk_io_empty_returns_none() {
        // 空输入 → 解析为 None
        assert!(parse_disk_io("").is_none());
    }

    #[test]
    fn parse_disk_io_non_numeric_falls_back_to_zero() {
        // 非数字字段应回退为 0.0 而非 panic
        let io = parse_disk_io("abc|def\n").expect("two fields");
        assert_eq!(io.read_bytes_per_sec, 0.0);
        assert_eq!(io.write_bytes_per_sec, 0.0);
    }

    #[test]
    fn parse_disk_io_single_field_returns_none() {
        // 只有一个字段（格式错）→ None
        assert!(parse_disk_io("100\n").is_none());
    }

    /// 契约测试：DeviceSnapshot 序列化键名必须与前端 StorageView/HardwareView 一致。
    #[test]
    fn device_snapshot_contract_keys() {
        let snap = DeviceSnapshot {
            input_devices: None,
            memory: MemInfo {
                total_gb: 32,
                available_gb: 14,
                used_percent: 58,
            },
            disks: vec![DiskItem {
                drive: "C".into(),
                total_gb: 487,
                free_gb: 41,
                used_percent: 91,
            }],
            disk_io: Some(DiskIo {
                read_bytes_per_sec: 1000.0,
                write_bytes_per_sec: 500.0,
            }),
        };
        let v = serde_json::to_value(&snap).unwrap();
        let obj = v.as_object().unwrap();
        // 顶层：前端读 device.disks / device.disk_io / device.memory
        assert!(obj.contains_key("memory"));
        assert!(obj.contains_key("disks"));
        assert!(obj.contains_key("disk_io"));
        // memory 子键：前端读 used_percent
        let mem = obj["memory"].as_object().unwrap();
        assert!(mem.contains_key("total_gb"));
        assert!(mem.contains_key("available_gb"));
        assert!(mem.contains_key("used_percent"));
        // disk 子键：前端读 drive/total_gb/free_gb/used_percent
        let disk = obj["disks"].as_array().unwrap()[0].as_object().unwrap();
        assert!(disk.contains_key("drive"));
        assert!(disk.contains_key("total_gb"));
        assert!(disk.contains_key("free_gb"));
        assert!(disk.contains_key("used_percent"));
        // disk_io 子键：前端读 read_bytes_per_sec/write_bytes_per_sec
        let io = obj["disk_io"].as_object().unwrap();
        assert!(io.contains_key("read_bytes_per_sec"));
        assert!(io.contains_key("write_bytes_per_sec"));
    }

    #[test]
    fn device_snapshot_disk_io_none_is_omitted() {
        // disk_io 为 None 时应省略该键（前端判断 != null）
        let snap = DeviceSnapshot {
            input_devices: None,
            memory: MemInfo {
                total_gb: 32,
                available_gb: 14,
                used_percent: 58,
            },
            disks: vec![],
            disk_io: None,
        };
        let v = serde_json::to_value(&snap).unwrap();
        assert!(
            !v.as_object().unwrap().contains_key("disk_io"),
            "disk_io 为 None 时应被省略"
        );
    }
}
