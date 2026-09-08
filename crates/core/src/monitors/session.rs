//! 会话状态监控（锁定/解锁/显示器变化）

use crate::types::*;
use serde_json::json;
use std::cell::Cell;
use std::time::Duration;
use windows_sys::Win32::Foundation::*;

pub struct SessionMonitor {
    was_locked: Cell<bool>,
    last_monitors: Cell<i32>,
}

impl Default for SessionMonitor {
    fn default() -> Self {
        Self {
            was_locked: Cell::new(false),
            last_monitors: Cell::new(0),
        }
    }
}

impl Monitor for SessionMonitor {
    fn name(&self) -> &str {
        "session"
    }

    fn interval(&self) -> Duration {
        Duration::from_secs(5)
    }

    fn collect(&self, tx: &crossbeam_channel::Sender<Event>) {
        // 检测锁定状态：OpenInputDesktop 在锁定时返回 NULL
        let is_locked = unsafe {
            let desktop = OpenInputDesktop(0, 0, 0x0001); // DESKTOP_READOBJECTS
            if !desktop.is_null() {
                CloseDesktop(desktop);
                false
            } else {
                true
            }
        };

        let prev_locked = self.was_locked.get();
        self.was_locked.set(is_locked);

        if is_locked && !prev_locked {
            let event =
                Event::new(EventAction::Lock, EventType::Session).data(json!({"locked": true}));
            let _ = tx.try_send(event);
        } else if !is_locked && prev_locked {
            let event =
                Event::new(EventAction::Unlock, EventType::Session).data(json!({"locked": false}));
            let _ = tx.try_send(event);
        }

        // 检测显示器数量变化
        let monitor_count = unsafe { GetSystemMetrics(SM_CMONITORS) };
        let prev_monitors = self.last_monitors.get();
        if prev_monitors > 0 && monitor_count != prev_monitors {
            let event = Event::new(EventAction::DisplayChange, EventType::Session).data(json!({
                "prev_count": prev_monitors,
                "current_count": monitor_count,
            }));
            let _ = tx.try_send(event);
        }
        self.last_monitors.set(monitor_count);
    }
}

use windows_sys::Win32::UI::WindowsAndMessaging::*;

const SM_CMONITORS: i32 = 80;

extern "system" {
    fn OpenInputDesktop(dwFlags: u32, fInherit: BOOL, dwDesiredAccess: u32) -> HANDLE;
    fn CloseDesktop(hDesktop: HANDLE) -> BOOL;
}
