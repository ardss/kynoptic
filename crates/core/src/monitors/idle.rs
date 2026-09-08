//! 空闲状态监控
//!
//! 使用 GetLastInputInfo 检测用户空闲时间，
//! 触发 IdleStart / IdleEnd 事件。

use crate::types::*;
use serde_json::json;
use std::cell::Cell;
use std::mem::zeroed;
use std::time::Duration;
use windows_sys::Win32::System::SystemInformation::GetTickCount64;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;

/// 空闲阈值：5 分钟无输入视为空闲
const IDLE_THRESHOLD_SECS: u64 = 300;

pub struct IdleMonitor {
    is_idle: Cell<bool>,
}

impl Default for IdleMonitor {
    fn default() -> Self {
        Self {
            is_idle: Cell::new(false),
        }
    }
}

impl Monitor for IdleMonitor {
    fn name(&self) -> &str {
        "idle"
    }

    fn interval(&self) -> Duration {
        Duration::from_secs(5)
    }

    fn collect(&self, tx: &crossbeam_channel::Sender<Event>) {
        let idle_seconds = get_idle_seconds();
        let now_idle = idle_seconds >= IDLE_THRESHOLD_SECS;
        let was_idle = self.is_idle.get();

        if now_idle && !was_idle {
            self.is_idle.set(true);
            let event = Event::new(EventAction::IdleStart, EventType::System).data(json!({
                "idle_seconds": idle_seconds,
                "threshold": IDLE_THRESHOLD_SECS,
            }));
            let _ = tx.try_send(event);
        } else if !now_idle && was_idle {
            self.is_idle.set(false);
            let event = Event::new(EventAction::IdleEnd, EventType::System).data(json!({
                "idle_seconds": idle_seconds,
                "threshold": IDLE_THRESHOLD_SECS,
            }));
            let _ = tx.try_send(event);
        }
    }
}

/// 获取自上次输入以来的空闲秒数
fn get_idle_seconds() -> u64 {
    unsafe {
        let mut lii: LASTINPUTINFO = zeroed();
        lii.cbSize = std::mem::size_of::<LASTINPUTINFO>() as u32;

        if GetLastInputInfo(&mut lii) == 0 {
            return 0;
        }

        let ticks = GetTickCount64();
        let idle_ms = ticks.saturating_sub(lii.dwTime as u64);
        idle_ms / 1000
    }
}
