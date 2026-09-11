//! 鼠标低级 Hook（SetWindowsHookExW WH_MOUSE_LL）
//!
//! raw 粒度（默认）：点击/滚轮/释放全部发送，移动每 500ms 采样一次。
//! minute 粒度（opt-in，见 collector::CollectorSettings）：回调退化为
//! 纯原子计数（input_agg::record_*），不做节流（原子操作无洪泛风险）。

use crate::types::*;
use crossbeam_channel::Sender;
use serde_json::json;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Mutex;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

const WH_MOUSE_LL: i32 = 14;
const WM_LBUTTONDOWN: u32 = 0x0201;
const WM_LBUTTONUP: u32 = 0x0202;
const WM_RBUTTONDOWN: u32 = 0x0204;
const WM_RBUTTONUP: u32 = 0x0205;
const WM_MBUTTONDOWN: u32 = 0x0207;
const WM_MBUTTONUP: u32 = 0x0208;
const WM_MOUSEWHEEL: u32 = 0x020A;
const WM_XBUTTONDOWN: u32 = 0x020B;
const WM_MOUSEMOVE: u32 = 0x0200;

/// 移动事件最小间隔（ms）
const MOVE_THROTTLE_MS: u64 = 500;

#[repr(C)]
#[allow(clippy::upper_case_acronyms)] // Win32 类型名,保持原样
struct POINT {
    x: i32,
    y: i32,
}

#[repr(C)]
#[allow(clippy::upper_case_acronyms)] // Win32 类型名,保持原样
struct MSLLHOOKSTRUCT {
    pt: POINT,
    mouse_data: u32,
    flags: u32,
    time: u32,
    dw_extra_info: usize,
}

// 可重启：使用 Mutex<Option<>> 而非 OnceLock，停止后可重新 set 新的 Sender。
static MOUSE_TX: Mutex<Option<Sender<Event>>> = Mutex::new(None);
static MOUSE_THREAD_ID: AtomicU32 = AtomicU32::new(0);
static MOUSE_HOOK: AtomicU32 = AtomicU32::new(0);
static LAST_MOVE_TIME: AtomicU64 = AtomicU64::new(0);

unsafe extern "system" fn mouse_proc(code: i32, wparam: usize, lparam: isize) -> isize {
    if code >= 0 {
        let ms = &*(lparam as *const MSLLHOOKSTRUCT);
        // minute 粒度：纯原子计数（最廉价路径），直接返回
        if crate::input_agg::minute_mode() {
            match wparam as u32 {
                WM_MOUSEMOVE => crate::input_agg::record_move(ms.pt.x, ms.pt.y),
                WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN | WM_XBUTTONDOWN => {
                    let button = match wparam as u32 {
                        WM_LBUTTONDOWN => 0,
                        WM_RBUTTONDOWN => 1,
                        WM_MBUTTONDOWN => 2,
                        // LL hook：X 按钮编号在 mouse_data 低字（1/2）
                        WM_XBUTTONDOWN => (ms.mouse_data & 0xffff).clamp(1, 2) as usize + 2,
                        _ => 0,
                    };
                    // LLMHF_INJECTED（0x1）：合成输入单独计数
                    let injected = ms.flags & 0x1 != 0;
                    crate::input_agg::record_click_button(button, injected);
                }
                WM_MOUSEWHEEL => {
                    let delta = (ms.mouse_data >> 16) as i16 as i32;
                    crate::input_agg::record_scroll((delta.unsigned_abs() / 120) as u64);
                }
                // 释放类事件只计样本，不影响点击计数口径
                _ => {}
            }
            return CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam);
        }

        // raw 粒度（默认）：原行为——克隆 Sender 后立即释放锁，避免在回调中长时间持锁。
        let tx = MOUSE_TX.lock().ok().and_then(|g| g.clone());
        if let Some(tx) = tx {
            match wparam as u32 {
                WM_MOUSEMOVE => {
                    // 节流：每 500ms 只发一次移动事件
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis() as u64;
                    let prev = LAST_MOVE_TIME.load(Ordering::Relaxed);
                    if now.saturating_sub(prev) < MOVE_THROTTLE_MS {
                        return CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam);
                    }
                    LAST_MOVE_TIME.store(now, Ordering::Relaxed);

                    let event = Event::new(EventAction::Move, EventType::Mouse).data(json!({
                        "x": ms.pt.x, "y": ms.pt.y,
                    }));
                    crate::collector::send_event(&tx, event);
                }
                WM_LBUTTONDOWN | WM_LBUTTONUP | WM_RBUTTONDOWN | WM_RBUTTONUP | WM_MBUTTONDOWN
                | WM_MBUTTONUP => {
                    let button = match wparam as u32 {
                        WM_LBUTTONDOWN | WM_LBUTTONUP => "left",
                        WM_RBUTTONDOWN | WM_RBUTTONUP => "right",
                        WM_MBUTTONDOWN | WM_MBUTTONUP => "middle",
                        _ => "unknown",
                    };
                    let pressed = matches!(
                        wparam as u32,
                        WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN
                    );
                    let action = if pressed {
                        EventAction::Click
                    } else {
                        EventAction::Release
                    };
                    let event = Event::new(action, EventType::Mouse).data(json!({
                        "button": button, "x": ms.pt.x, "y": ms.pt.y, "pressed": pressed,
                    }));
                    crate::collector::send_event(&tx, event);
                }
                WM_MOUSEWHEEL => {
                    let delta = (ms.mouse_data >> 16) as i16 as i32;
                    let direction = if delta > 0 { "up" } else { "down" };
                    let steps = delta.unsigned_abs() / 120;
                    let event = Event::new(EventAction::Scroll, EventType::Mouse).data(json!({
                        "x": ms.pt.x, "y": ms.pt.y,
                        "direction": direction, "steps": steps, "dy": delta,
                    }));
                    crate::collector::send_event(&tx, event);
                }
                _ => {}
            }
        }
    }
    CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam)
}

pub struct MouseHook;

impl Default for MouseHook {
    fn default() -> Self {
        Self
    }
}

unsafe impl Send for MouseHook {}

impl EventHook for MouseHook {
    fn start(&self, tx: Sender<Event>) {
        if let Ok(mut g) = MOUSE_TX.lock() {
            *g = Some(tx);
        }
        std::thread::Builder::new()
            .name("mouse_hook".into())
            .spawn(|| unsafe {
                MOUSE_THREAD_ID.store(
                    windows_sys::Win32::System::Threading::GetCurrentThreadId(),
                    Ordering::Release,
                );

                let hook =
                    SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), std::ptr::null_mut(), 0);
                if hook.is_null() {
                    log::error!("鼠标 Hook 安装失败");
                    MOUSE_THREAD_ID.store(0, Ordering::Release);
                    return;
                }
                MOUSE_HOOK.store(hook as u32, Ordering::Release);
                log::info!("mouse_hook 已启动 (事件驱动, 移动节流 500ms)");

                let mut msg: MSG = std::mem::zeroed();
                while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) != 0 {
                    TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }

                UnhookWindowsHookEx(hook);
                MOUSE_HOOK.store(0, Ordering::Release);
                MOUSE_THREAD_ID.store(0, Ordering::Release);
                log::info!("mouse_hook 已停止");
            })
            .expect("mouse hook thread");
    }

    fn stop(&self) {
        let tid = MOUSE_THREAD_ID.load(Ordering::Acquire);
        if tid != 0 {
            unsafe {
                PostThreadMessageW(tid, WM_QUIT, 0, 0);
            }
        }
    }
}
