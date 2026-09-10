//! 键盘低级 Hook（SetWindowsHookExW WH_KEYBOARD_LL）

use crate::types::*;
use crossbeam_channel::Sender;
use serde_json::json;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

const WH_KEYBOARD_LL: i32 = 13;
const WM_KEYDOWN: u32 = 0x0100;
const WM_KEYUP: u32 = 0x0101;
const WM_SYSKEYDOWN: u32 = 0x0104;
const WM_SYSKEYUP: u32 = 0x0105;

#[repr(C)]
#[allow(clippy::upper_case_acronyms)] // Win32 类型名,保持原样
struct KBDLLHOOKSTRUCT {
    vk_code: u32,
    scan_code: u32,
    flags: u32,
    time: u32,
    dw_extra_info: usize,
}

// 可重启：使用 Mutex<Option<>> 而非 OnceLock，停止后可重新 set 新的 Sender。
static KB_TX: Mutex<Option<Sender<Event>>> = Mutex::new(None);
static KB_THREAD_ID: AtomicU32 = AtomicU32::new(0);
static KB_HOOK: AtomicU32 = AtomicU32::new(0);

unsafe extern "system" fn keyboard_proc(code: i32, wparam: usize, lparam: isize) -> isize {
    if code >= 0 {
        // minute 粒度（opt-in）：纯原子计数，跳过修饰键采样与事件构造
        if crate::input_agg::minute_mode() {
            if matches!(wparam as u32, WM_KEYDOWN | WM_SYSKEYDOWN) {
                let vk = unsafe { &*(lparam as *const KBDLLHOOKSTRUCT) }.vk_code;
                crate::input_agg::record_key_vk(vk);
            }
            return CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam);
        }

        // raw 粒度（默认）：原行为——克隆 Sender 后立即释放锁，避免在回调中长时间持锁。
        let tx = KB_TX.lock().ok().and_then(|g| g.clone());
        if let Some(tx) = tx {
            let kb = &*(lparam as *const KBDLLHOOKSTRUCT);
            let action = match wparam as u32 {
                WM_KEYDOWN | WM_SYSKEYDOWN => EventAction::Press,
                WM_KEYUP | WM_SYSKEYUP => EventAction::Release,
                _ => return CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam),
            };

            // 检测修饰键状态（GetAsyncKeyState 返回 i16，最高位表示按下）
            let shift = GetAsyncKeyState(0x10) < 0;
            let ctrl = GetAsyncKeyState(0x11) < 0;
            let alt = GetAsyncKeyState(0x12) < 0;
            let win = GetAsyncKeyState(0x5B) < 0 || GetAsyncKeyState(0x5C) < 0;

            let mut modifiers = Vec::new();
            if ctrl {
                modifiers.push("ctrl");
            }
            if alt {
                modifiers.push("alt");
            }
            if shift {
                modifiers.push("shift");
            }
            if win {
                modifiers.push("win");
            }

            let event = Event::new(action, EventType::Keyboard).data(json!({
                "vk_code": kb.vk_code,
                "scan_code": kb.scan_code,
                "flags": kb.flags,
                "modifiers": modifiers,
            }));
            crate::collector::send_event(&tx, event);
        }
    }
    CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam)
}

extern "system" {
    fn GetAsyncKeyState(vkey: i32) -> i16;
}

pub struct KeyboardHook;

impl Default for KeyboardHook {
    fn default() -> Self {
        Self
    }
}

unsafe impl Send for KeyboardHook {}

impl EventHook for KeyboardHook {
    fn start(&self, tx: Sender<Event>) {
        if let Ok(mut g) = KB_TX.lock() {
            *g = Some(tx);
        }
        std::thread::Builder::new()
            .name("keyboard_hook".into())
            .spawn(|| unsafe {
                KB_THREAD_ID.store(
                    windows_sys::Win32::System::Threading::GetCurrentThreadId(),
                    Ordering::Release,
                );

                let hook =
                    SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_proc), std::ptr::null_mut(), 0);
                if hook.is_null() {
                    log::error!("键盘 Hook 安装失败");
                    KB_THREAD_ID.store(0, Ordering::Release);
                    return;
                }
                KB_HOOK.store(hook as u32, Ordering::Release);
                log::info!("keyboard_hook 已启动 (事件驱动)");

                let mut msg: MSG = std::mem::zeroed();
                while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) != 0 {
                    TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }

                UnhookWindowsHookEx(hook);
                KB_HOOK.store(0, Ordering::Release);
                KB_THREAD_ID.store(0, Ordering::Release);
                log::info!("keyboard_hook 已停止");
            })
            .expect("keyboard hook thread");
    }

    fn stop(&self) {
        let tid = KB_THREAD_ID.load(Ordering::Acquire);
        if tid != 0 {
            unsafe {
                PostThreadMessageW(tid, WM_QUIT, 0, 0);
            }
        }
    }
}
