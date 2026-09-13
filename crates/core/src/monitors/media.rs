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

/// 已知会使用摄像头/麦克风的通信应用（全部小写，用于包含匹配）
const MEDIA_APPS: &[&str] = &[
    "zoom.exe",
    "teams.exe",
    "skype.exe",
    "discord.exe",
    "wechat.exe",
    "weixin.exe",
    "weixinapp.exe",
    "feishu.exe",
    "wemeetapp.exe",
    "dingtalk.exe",
    "lark.exe",
    "webexhost.exe",
    "obs64.exe",
    "obs32.exe",
    "vlc.exe",
    "viber.exe",
    "linphone.exe",
    "googlemeet.exe",
    "qqmusic.exe",
    "kugou.exe",
    "cloudmusic.exe",
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
                let lower = name.to_lowercase();
                if is_media_app(&lower) {
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

/// 小写包含匹配：进程名（去掉 .exe 后缀）包含任一已知媒体应用名即命中
/// （覆盖带变体后缀的进程，如 WemeetApp_x64.exe、Feishu_Update.exe、
/// Netease_CloudMusic.exe 等；调用方须先 to_lowercase）
fn is_media_app(name_lower: &str) -> bool {
    let stem = name_lower.strip_suffix(".exe").unwrap_or(name_lower);
    MEDIA_APPS
        .iter()
        .any(|app| stem.contains(app.strip_suffix(".exe").unwrap_or(app)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_known_media_apps() {
        assert!(is_media_app("zoom.exe"));
        assert!(is_media_app("obs64.exe"));
        assert!(is_media_app("wechat.exe"));
    }

    #[test]
    fn matches_new_chinese_apps() {
        assert!(is_media_app("weixin.exe"));
        assert!(!is_media_app("Weixin.exe")); // 大写须先转小写再匹配
        assert!(is_media_app("feishu.exe"));
        assert!(is_media_app("wemeetapp.exe"));
        assert!(is_media_app("dingtalk.exe"));
        assert!(is_media_app("qqmusic.exe"));
        assert!(is_media_app("kugou.exe"));
        assert!(is_media_app("cloudmusic.exe"));
        assert!(is_media_app("netease_cloudmusic.exe"));
    }

    #[test]
    fn matches_variant_names_by_containment() {
        assert!(is_media_app("wemeetapp_x64.exe"));
        assert!(is_media_app("feishu_update.exe"));
        assert!(is_media_app("obs64.1.exe"));
    }

    #[test]
    fn does_not_match_unrelated() {
        assert!(!is_media_app("explorer.exe"));
        assert!(!is_media_app("code.exe"));
        assert!(!is_media_app(""));
    }
}
