//! 媒体设备使用监控
//!
//! 检查已知通信/媒体应用进程名来判断摄像头/麦克风使用状态。
//! 仅在活跃应用列表变化时发送事件。

use crate::types::*;
use serde_json::json;
use std::cell::Cell;
use std::collections::HashSet;
use std::mem::{size_of, zeroed};
use std::time::Duration;
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::System::Diagnostics::ToolHelp::*;

/// 已知会使用摄像头/麦克风的通信应用（全部小写，用于匹配）
const MEDIA_APPS: &[&str] = &[
    "zoom.exe",
    "teams.exe",
    "skype.exe",
    "discord.exe",
    "wechat.exe",
    "dingtalk.exe",
    "lark.exe",
    "webexhost.exe",
    "obs64.exe",
    "obs32.exe",
    "vlc.exe",
    "viber.exe",
    "linphone.exe",
    "googlemeet.exe",
];

pub struct MediaMonitor {
    prev_active: Cell<Option<HashSet<String>>>,
}

impl Default for MediaMonitor {
    fn default() -> Self {
        Self {
            prev_active: Cell::new(None),
        }
    }
}

impl Monitor for MediaMonitor {
    fn name(&self) -> &str {
        "media"
    }
    fn interval(&self) -> Duration {
        Duration::from_secs(5)
    }

    fn collect(&self, tx: &crossbeam_channel::Sender<Event>) {
        let active = detect_media_apps();
        let active_set: HashSet<String> = active.iter().cloned().collect();

        let prev = self.prev_active.take();
        match prev {
            None => {
                // 首次：只记录，不发送空列表
                if !active.is_empty() {
                    let event = Event::new(EventAction::MediaDevice, EventType::System)
                        .data(json!({ "active_media_apps": active, "count": active.len() }));
                    let _ = tx.try_send(event);
                }
            }
            Some(prev_set) => {
                // 只在变化时发送
                if active_set != prev_set {
                    let event = Event::new(EventAction::MediaDevice, EventType::System)
                        .data(json!({ "active_media_apps": active, "count": active.len() }));
                    let _ = tx.try_send(event);
                }
            }
        }
        self.prev_active.set(Some(active_set));
    }
}

fn detect_media_apps() -> Vec<String> {
    let media_set: HashSet<&str> = MEDIA_APPS.iter().copied().collect();
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
                if media_set.contains(name.to_lowercase().as_str()) {
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
