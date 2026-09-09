//! 浏览器标签页监控
//!
//! 检测前台窗口是否为浏览器进程，提取页面标题。

use crate::types::*;
use serde_json::json;
use std::cell::Cell;
use std::time::Duration;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

/// 已知浏览器可执行文件名
const BROWSER_EXES: &[&str] = &[
    "chrome.exe",
    "firefox.exe",
    "msedge.exe",
    "opera.exe",
    "brave.exe",
    "vivaldi.exe",
    "iexplore.exe",
    "browser.exe",
    "maxthon.exe",
    "thorium.exe",
    // 国内常见浏览器
    "360chrome.exe",
    "360se.exe",
    "qqbrowser.exe",
    "sogouexplorer.exe",
    "liebao.exe",
    "ucbrowser.exe",
    "2345explorer.exe",
    "haochrome.exe",
    "avguate.exe",
    "saayaa.exe",
    "twchrome.exe",
    "chitubox.exe",
    "centbrowser.exe",
    "yandex.exe",
];

pub struct BrowserMonitor {
    last_title: Cell<String>,
}

impl Default for BrowserMonitor {
    fn default() -> Self {
        Self {
            last_title: Cell::new(String::new()),
        }
    }
}

// SAFETY: BrowserMonitor 内部用 Cell<String>，String 是 Send，
// Cell<String> 在单线程轮询中使用是安全的。
unsafe impl Send for BrowserMonitor {}

impl Monitor for BrowserMonitor {
    fn name(&self) -> &str {
        "browser"
    }

    fn interval(&self) -> Duration {
        Duration::from_secs(5)
    }

    fn collect(&self, tx: &crossbeam_channel::Sender<Event>) {
        unsafe {
            let hwnd = GetForegroundWindow();
            if hwnd.is_null() {
                return;
            }

            let mut pid: u32 = 0;
            GetWindowThreadProcessId(hwnd, &mut pid);
            if pid == 0 {
                return;
            }

            let proc_name = get_process_name(pid);
            if proc_name.is_empty() {
                return;
            }

            let is_browser = BROWSER_EXES
                .iter()
                .any(|b| b.eq_ignore_ascii_case(&proc_name));

            if !is_browser {
                return;
            }

            let mut buf = [0u16; 512];
            let len = GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32);
            let title = String::from_utf16_lossy(&buf[..len as usize]);

            let prev_title = self.last_title.take();
            if title == prev_title {
                self.last_title.set(prev_title);
                return;
            }
            self.last_title.set(title.clone());

            let event = Event::new(EventAction::TabChange, EventType::Window)
                .data(json!({
                    "browser": proc_name,
                    "title": title,
                    "pid": pid,
                }))
                .app(&proc_name, &title);

            let _ = tx.try_send(event);
        }
    }
}

/// 通过 pid 查询进程可执行文件名。
///
/// 使用 QueryFullProcessImageNameW 直接获取完整路径（O(1)），
/// 替代原先对整个进程列表做 CreateToolhelp32Snapshot 线性扫描（O(所有进程)）。
/// 输出为纯文件名（如 "chrome.exe"），与原实现一致。
fn get_process_name(pid: u32) -> String {
    use windows_sys::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return String::new();
        }

        let mut buf = [0u16; 520];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(handle, 0, buf.as_mut_ptr(), &mut len);
        windows_sys::Win32::Foundation::CloseHandle(handle);

        if ok == 0 {
            return String::new();
        }

        let full = String::from_utf16_lossy(&buf[..len as usize]);
        // 提取文件名部分（去路径 + 去扩展名保持原样，如 "chrome.exe"）
        full.rsplit('\\').next().unwrap_or(&full).to_string()
    }
}
