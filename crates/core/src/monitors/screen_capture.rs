//! 截屏/录屏检测
//!
//! 通过进程名扫描检测已知录屏/截图工具的运行状态。

use crate::types::*;
use serde_json::json;
use std::cell::Cell;
use std::collections::HashSet;
use std::mem::{size_of, zeroed};
use std::time::Duration;
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::System::Diagnostics::ToolHelp::*;

/// 已知录屏/截图工具进程名
const SCREEN_CAPTURE_PROCS: &[&str] = &[
    "obs64.exe",
    "obs32.exe",
    "ffmpeg.exe",
    "vlc.exe",
    "camtasia.exe",
    "snagit.exe",
    "lightshot.exe",
    "greenshot.exe",
    "sharex.exe",
    "screenpresso.exe",
    "bandicam.exe",
    "fraps.exe",
    "xsplit.exe",
    "loom.exe",
    "recordcast.exe",
    "screencastify.exe",
    "captura.exe",
    "shutter.exe",
];

pub struct ScreenCaptureMonitor {
    prev_active: Cell<bool>,
}

impl Default for ScreenCaptureMonitor {
    fn default() -> Self {
        Self {
            prev_active: Cell::new(false),
        }
    }
}

impl Monitor for ScreenCaptureMonitor {
    fn name(&self) -> &str {
        "screen_capture"
    }
    fn interval(&self) -> Duration {
        Duration::from_secs(10)
    }

    fn collect(&self, tx: &crossbeam_channel::Sender<Event>) {
        let active_procs = detect_screen_capture();
        let now_active = !active_procs.is_empty();
        let was_active = self.prev_active.get();

        if now_active != was_active {
            self.prev_active.set(now_active);
            let event = Event::new(EventAction::ScreenCapture, EventType::System).data(json!({
                "type": "recording",
                "active": now_active,
                "processes": active_procs,
            }));
            let _ = tx.try_send(event);
        }
    }
}

fn detect_screen_capture() -> Vec<String> {
    let proc_set: HashSet<&str> = SCREEN_CAPTURE_PROCS.iter().copied().collect();
    let mut result = Vec::new();

    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return result;
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
                if proc_set.contains(name.to_lowercase().as_str()) {
                    result.push(name);
                }
                if Process32NextW(snapshot, &mut entry) == 0 {
                    break;
                }
            }
        }

        CloseHandle(snapshot);
    }

    result
}
