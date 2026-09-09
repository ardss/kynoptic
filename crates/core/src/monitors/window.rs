//! 前台窗口切换监控

use crate::types::*;
use serde_json::json;
use std::cell::Cell;
use std::time::Duration;
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

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

    fn interval(&self) -> Duration {
        Duration::from_secs(1)
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
            log::debug!("window: foreground changed to {:?}", hwnd);
            self.last_hwnd.set(hwnd);

            // 获取窗口标题
            let mut buf = [0u16; 512];
            let len = GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32);
            let title = String::from_utf16_lossy(&buf[..len as usize]);

            // 获取进程 ID 与进程名（app_name 铁律：不得留空）
            let mut pid: u32 = 0;
            GetWindowThreadProcessId(hwnd, &mut pid);
            let proc_name = super::browser::get_process_name(pid);

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
