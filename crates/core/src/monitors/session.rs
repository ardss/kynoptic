//! 会话状态监控（锁定/解锁/显示器变化）

use crate::types::*;
use serde_json::json;
use std::cell::Cell;
use std::time::Duration;
use windows_sys::Win32::Foundation::*;

pub struct SessionMonitor {
    was_locked: Cell<bool>,
    last_monitors: Cell<i32>,
    /// 连续"测得锁屏"的轮数（去抖计数，见 collect 内注释）
    locked_streak: Cell<u32>,
    /// 连续"测得解锁"的轮数（去抖计数）
    unlocked_streak: Cell<u32>,
}

impl Default for SessionMonitor {
    fn default() -> Self {
        Self {
            was_locked: Cell::new(false),
            last_monitors: Cell::new(0),
            locked_streak: Cell::new(0),
            unlocked_streak: Cell::new(0),
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
        // 检测锁定状态：OpenInputDesktop 在锁定时返回 NULL。Win32 语义：
        // 输入桌面在锁屏/UAC 安全桌面（WinSta0\Winlogon 之外的 Secure 桌面）
        // 激活期间不开放——因此 UAC 同意弹窗也会得到 NULL，属于已知误报源；
        // 且本监控器 5s 轮询一次，<5s 的瞬时锁定可能整段落在两次采样之间
        // 而被漏记（接受，见下）。
        let is_locked = unsafe {
            let desktop = OpenInputDesktop(0, 0, 0x0001); // DESKTOP_READOBJECTS
            if !desktop.is_null() {
                CloseDesktop(desktop);
                false
            } else {
                true
            }
        };

        // 去抖（审查 LOW）：单拍 NULL 不足以翻转状态——UAC 弹窗往往只存在
        // 一两个轮询周期，误报 Lock 会污染会话统计。要求同一观测连续两拍
        // （2 × 5s 间隔 = 约 10s）才发 Lock/Unlock，瞬态安全桌面切换被自然
        // 滤除；代价是 <10s 的真实锁/解可能整段漏记（接受）。
        if is_locked {
            self.locked_streak.set(self.locked_streak.get() + 1);
            self.unlocked_streak.set(0);
        } else {
            self.unlocked_streak.set(self.unlocked_streak.get() + 1);
            self.locked_streak.set(0);
        }

        let prev_locked = self.was_locked.get();
        if !prev_locked && self.locked_streak.get() >= 2 {
            self.was_locked.set(true);
            let event =
                Event::new(EventAction::Lock, EventType::Session).data(json!({"locked": true}));
            let _ = tx.try_send(event);
        } else if prev_locked && self.unlocked_streak.get() >= 2 {
            self.was_locked.set(false);
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
