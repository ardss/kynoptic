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

            // 标题脱敏（opt-in，见 title_privacy）：默认原样，开启后剥 URL 查询串。
            // 去重比较仍用原文标题（避免脱敏后不同页面误判为同页）。
            let title = super::title_privacy::redact_title(&title);

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

/// 经 TH32 快照无法解析（进程已退出）且 OpenProcess 也被拒（高权限进程，
/// UAC 提权窗口属主句柄不可 open）时的兜底应用名标记：app_name 永不为空
/// （发现 platform high：空 app_name 进 SQL 后 COALESCE 归 (unknown)，
/// 真实活动被静默吞掉）。
pub(crate) const ELEVATED_APP_MARKER: &str = "<elevated>";

/// 通过 pid 查询进程可执行文件名。
///
/// 两级解析（发现 platform high 修复：高权限进程 OpenProcess 被 UAC 拒绝
/// 时旧实现返回 ""，事件 app_name 为空 → 展示层归 (unknown) 吞掉活动）：
/// 1. 主路径 OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION) +
///    QueryFullProcessImageNameW：同权限进程 O(1) 直接命中。
/// 2. 兜底 TH32 快照按 pid 查（探针实测：低权限 TH32 可直接读到
///    System/lsass/conhost 等高权限进程的 exe 名，无需句柄）。
/// 3. pid 已退出（快照里查无此进程）→ [`ELEVATED_APP_MARKER`] 标记，
///    保持 app_name 非空。
///
/// 输出为纯文件名（如 "chrome.exe"），与原实现一致。
pub(crate) fn get_process_name(pid: u32) -> String {
    let name = open_process_name(pid);
    if !name.is_empty() {
        return name;
    }
    match th32_lookup_name(pid) {
        Some(n) => n,
        None => ELEVATED_APP_MARKER.to_string(),
    }
}

/// 主路径：OpenProcess + QueryFullProcessImageNameW（纯文件名）。
fn open_process_name(pid: u32) -> String {
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

/// 兜底路径：TH32 进程快照按 pid 查 exe 名（低权限可读高权限进程名，
/// 探针实测 System/lsass/conhost 均可读；无需 OpenProcess 句柄）。
/// 快照失败或 pid 不在快照（已退出）→ None。
fn th32_lookup_name(pid: u32) -> Option<String> {
    use std::mem::{size_of, zeroed};
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::System::Diagnostics::ToolHelp::*;

    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return None;
        }
        let mut entry: PROCESSENTRY32W = zeroed();
        entry.dwSize = size_of::<PROCESSENTRY32W>() as u32;
        let mut found = None;
        if Process32FirstW(snapshot, &mut entry) != 0 {
            loop {
                if entry.th32ProcessID == pid {
                    let end = entry
                        .szExeFile
                        .iter()
                        .position(|&c| c == 0)
                        .unwrap_or(entry.szExeFile.len());
                    found = Some(String::from_utf16_lossy(&entry.szExeFile[..end]));
                    break;
                }
                if Process32NextW(snapshot, &mut entry) == 0 {
                    break;
                }
            }
        }
        windows_sys::Win32::Foundation::CloseHandle(snapshot);
        found.filter(|n| !n.is_empty())
    }
}

/// 通过 pid 查询进程完整路径（OpenProcess + QueryFullProcessImageNameW）。
/// 高权限进程/进程已退出 → None（调用方自行降级，宿主解析据此回退）。
pub(crate) fn process_full_path(pid: u32) -> Option<String> {
    use windows_sys::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return None;
        }
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(handle, 0, buf.as_mut_ptr(), &mut len);
        windows_sys::Win32::Foundation::CloseHandle(handle);
        if ok == 0 {
            return None;
        }
        let full = String::from_utf16_lossy(&buf[..len as usize]);
        if full.is_empty() {
            return None;
        }
        Some(full)
    }
}
