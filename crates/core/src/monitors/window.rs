//! 前台窗口切换监控

use crate::types::*;
use serde_json::json;
use std::cell::Cell;
use std::time::Duration;
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

/// 本进程 exe 文件名（小写，如 "kynoptic.exe" / "kynoptic-tray.exe"），
/// 进程生命周期内不变，缓存一次。取不到返回 None（不排除任何东西）。
fn self_exe_name() -> Option<String> {
    use std::sync::OnceLock;
    static CACHE: OnceLock<Option<String>> = OnceLock::new();
    CACHE
        .get_or_init(|| {
            std::env::current_exe()
                .ok()
                .map(|p| normalize_exe_name(&p.to_string_lossy()))
        })
        .clone()
}

/// 规范化 exe 名用于比较：取文件名、统一小写、统一斜杠。
/// 纯函数，单测覆盖（不依赖真实进程）。
fn normalize_exe_name(path: &str) -> String {
    let name = path.rsplit(['\\', '/']).next().unwrap_or(path);
    name.trim_matches('"').to_ascii_lowercase()
}

/// 是否为本采集器进程自身的 exe（kynoptic.exe / kynoptic-tray.exe 等——
/// 本 crate 编译出的任一 bin 都会被比对命中）。纯函数，单测覆盖。
///
/// 排除原因：采集器/托盘自己也会成为前台窗口（托盘弹菜单、以窗口形式
/// 短暂出现的辅助进程），它们产生的 window/switch 事件会以 "kynoptic.exe"
/// 污染 Top 应用排行，首装空库时甚至让 TOP 应用只有采集器 owner 自己。
/// 数据口径：我们度量的是"用户在用电脑干什么"，采集器自身不属于该口径。
fn exe_matches_self(self_exe: &str, other_proc: &str) -> bool {
    if self_exe.is_empty() || other_proc.is_empty() {
        return false;
    }
    normalize_exe_name(other_proc) == normalize_exe_name(self_exe)
}

pub struct WindowMonitor {
    last_hwnd: Cell<HWND>,
}

// SAFETY: HWND 是不透明句柄（指针），我们只用它做相等比较，
// 不通过它在线程间共享可变数据。Cell<HWND> 可以安全 Send。
unsafe impl Send for WindowMonitor {}

impl Default for WindowMonitor {
    fn default() -> Self {
        Self {
            last_hwnd: Cell::new(std::ptr::null_mut()),
        }
    }
}

impl Monitor for WindowMonitor {
    fn name(&self) -> &str {
        "window"
    }

    // 约束（采样口径）：本监控是轮询式而非事件驱动（无
    // SetWinEventHook/EVENT_SYSTEM_FOREGROUND），寿命短于一个采样周期的
    // 前台切换（通知弹窗、广告抢焦等）可能整段漏记；恰好跨过采样点的
    // 会被记录一次。曾为 1s，现降到 200ms 缩小漏记窗口——彻底消除需改用
    // WinEventProc 事件钩子并重接 monitor 调度框架，非本轮最小改动范围。
    fn interval(&self) -> Duration {
        Duration::from_millis(200)
    }

    fn collect(&self, tx: &crossbeam_channel::Sender<Event>) {
        unsafe {
            let hwnd = GetForegroundWindow();
            if hwnd.is_null() {
                return;
            }

            // 快速路径：hwnd 未变，直接返回
            if hwnd == self.last_hwnd.get() {
                return;
            }
            self.last_hwnd.set(hwnd);

            // 获取窗口标题
            let mut buf = [0u16; 512];
            let len = GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32);
            let title = String::from_utf16_lossy(&buf[..len as usize]);

            // 获取进程 ID 与进程名（app_name 铁律：不得留空）
            let mut pid: u32 = 0;
            GetWindowThreadProcessId(hwnd, &mut pid);
            let proc_name = super::browser::get_process_name(pid);

            // 自身进程排除：last_hwnd 已更新（避免每秒重复比对），
            // 但不产生事件——见 exe_matches_self 的注释。
            if self_exe_name()
                .map(|s| exe_matches_self(&s, &proc_name))
                .unwrap_or(false)
            {
                log::debug!("window: skip self process ({proc_name})");
                return;
            }

            log::debug!("window: foreground changed to {:?}", hwnd);

            // 标题脱敏（opt-in，见 title_privacy）：默认原样，开启后剥 URL 查询串
            let title = super::title_privacy::redact_title(&title);

            let event = Event::new(EventAction::Switch, EventType::Window)
                .data(json!({
                    "hwnd": hwnd as u64,
                    "title": title,
                    "pid": pid,
                    "proc": proc_name,
                }))
                .app(&proc_name, &title);

            let _ = tx.try_send(event);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_exe_name_handles_slashes_quotes_case() {
        assert_eq!(
            normalize_exe_name(r"C:\apps\Kynoptic\kynoptic.exe"),
            "kynoptic.exe"
        );
        assert_eq!(normalize_exe_name("kynoptic-tray.exe"), "kynoptic-tray.exe");
        assert_eq!(normalize_exe_name("/usr/bin/KYNOPTIC"), "kynoptic");
        assert_eq!(normalize_exe_name(r#""C:\x\kynoptic.exe""#), "kynoptic.exe");
    }

    #[test]
    fn self_process_excluded_for_all_own_bins() {
        // 本 crate 编译出的任一 bin（采集器/托盘/ctl）在作为 current_exe 时
        // 都不该进前台事件；且每个 bin 只匹配自己的名字，不误伤兄弟 bin。
        for own in ["kynoptic.exe", "kynoptic-tray.exe", "kynoptic-ctl.exe"] {
            assert!(exe_matches_self(own, own), "{own} 应与自身匹配");
            for other in ["kynoptic.exe", "kynoptic-tray.exe", "kynoptic-ctl.exe"] {
                if other != own {
                    assert!(
                        !exe_matches_self(other, own),
                        "{own} 不应误伤兄弟进程 {other}"
                    );
                }
            }
        }
        // 大小写与引号路径归一化后仍应命中自身（kynoptic.exe 归一化即此名）
        assert!(exe_matches_self(
            "kynoptic.exe",
            r"C:\Program Files\Kynoptic\KYNOPTIC.EXE"
        ));
        assert!(exe_matches_self("kynoptic.exe", r#""C:\x\kynoptic.exe""#));
    }

    #[test]
    fn foreign_processes_never_excluded() {
        assert!(!exe_matches_self("kynoptic.exe", "chrome.exe"));
        assert!(!exe_matches_self("kynoptic.exe", "kynoptic-viewer.exe"));
        assert!(!exe_matches_self("kynoptic.exe", "notkynoptic.exe"));
    }

    #[test]
    fn empty_names_are_never_self() {
        // proc 名取不到（OpenProcess 失败返回空串）时不得连带排除真实事件
        assert!(!exe_matches_self("kynoptic.exe", ""));
        assert!(!exe_matches_self("", "kynoptic.exe"));
    }
}
