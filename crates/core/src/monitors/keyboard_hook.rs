//! 键盘低级 Hook（SetWindowsHookExW WH_KEYBOARD_LL）

use crate::types::*;
use crossbeam_channel::Sender;
use serde_json::json;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
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
/// 最近一次键盘事件的 epoch 毫秒（Wave20 P0：摘钩检测用——LL hook 被
/// 系统超时摘除时静默死亡，这里提供"还在产事件吗"的信号）
static KB_LAST_EVENT_MS: AtomicU64 = AtomicU64::new(0);
/// hook 线程自定义消息：重装 hook（摘钩自愈）
const WM_APP_REHOOK: u32 = 0x8105;

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

unsafe extern "system" fn keyboard_proc(code: i32, wparam: usize, lparam: isize) -> isize {
    if code >= 0 {
        KB_LAST_EVENT_MS.store(now_ms(), Ordering::Relaxed);
        // minute 粒度（opt-in）：纯原子计数，跳过修饰键采样与事件构造
        if crate::input_agg::minute_mode() {
            if matches!(wparam as u32, WM_KEYDOWN | WM_SYSKEYDOWN) {
                let kb = unsafe { &*(lparam as *const KBDLLHOOKSTRUCT) };
                // LLKHF_INJECTED（0x10）：SendKeys/SendInput 等合成输入，
                // 单独计数供"人在场 vs 自动化活动"分离
                let injected = kb.flags & 0x10 != 0;
                crate::input_agg::record_key_vk(kb.vk_code, injected);
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

/// 取出静态槽位中的 Sender（取出即从槽位移走，drop 时通道断开）。
/// stop() 与测试共用：channel Disconnected 是 collector writer join 的唯一
/// 退出条件（见 collector::writer_loop），所以关停必须让 sender 真正 drop。
fn take_kb_tx() -> Option<Sender<Event>> {
    KB_TX.lock().ok().and_then(|mut g| g.take())
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
                let my_tid = windows_sys::Win32::System::Threading::GetCurrentThreadId();
                KB_THREAD_ID.store(my_tid, Ordering::Release);

                let mut hook =
                    SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_proc), std::ptr::null_mut(), 0);
                if hook.is_null() {
                    log::error!("键盘 Hook 安装失败");
                    let _ = KB_THREAD_ID.compare_exchange(
                        my_tid,
                        0,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    );
                    return;
                }
                KB_HOOK.store(hook as u32, Ordering::Release);
                log::info!("keyboard_hook 已启动 (事件驱动)");

                // Wave20 P0 摘钩自愈：监视线程发现"光标在动但本 hook 长时间
                // 无事件"（LL hook 被系统 LowLevelHooksTimeout 静默摘除的
                // 特征）时投递 WM_APP_REHOOK，在本线程（消息循环所在线程）
                // 重装。误报无害（先摘后装，不产生双 hook）。
                {
                    std::thread::Builder::new()
                        .name("keyboard_hook_watch".into())
                        .spawn(move || {
                            #[repr(C)]
                            #[allow(clippy::upper_case_acronyms)]
                            struct POINT {
                                x: i32,
                                y: i32,
                            }
                            extern "system" {
                                fn GetCursorPos(pt: *mut POINT) -> i32;
                            }
                            let mut prev = POINT { x: 0, y: 0 };
                            let _ = GetCursorPos(&mut prev);
                            loop {
                                std::thread::sleep(std::time::Duration::from_secs(10));
                                if KB_THREAD_ID.load(Ordering::Acquire) != my_tid {
                                    return; // 本代 hook 已停止
                                }
                                let mut cur = POINT { x: 0, y: 0 };
                                if GetCursorPos(&mut cur) == 0 {
                                    continue;
                                }
                                let moved = cur.x != prev.x || cur.y != prev.y;
                                prev = cur;
                                let stale = now_ms()
                                    .saturating_sub(KB_LAST_EVENT_MS.load(Ordering::Relaxed))
                                    > 30_000;
                                if moved && stale {
                                    log::warn!("keyboard_hook 疑似被系统摘除，尝试重装");
                                    PostThreadMessageW(my_tid, WM_APP_REHOOK, 0, 0);
                                }
                            }
                        })
                        .ok();
                }

                let mut msg: MSG = std::mem::zeroed();
                loop {
                    let r = GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0);
                    if r <= 0 {
                        if r == -1 {
                            // GetMessageW 出错返回 -1，旧写法 !=0 会带着错误
                            // 状态空转烧 CPU（Wave20 P1）
                            std::thread::sleep(std::time::Duration::from_millis(50));
                            continue;
                        }
                        break; // WM_QUIT
                    }
                    if msg.message == WM_APP_REHOOK {
                        UnhookWindowsHookEx(hook);
                        let h = SetWindowsHookExW(
                            WH_KEYBOARD_LL,
                            Some(keyboard_proc),
                            std::ptr::null_mut(),
                            0,
                        );
                        if !h.is_null() {
                            hook = h;
                            KB_HOOK.store(h as u32, Ordering::Release);
                            KB_LAST_EVENT_MS.store(now_ms(), Ordering::Relaxed);
                            log::info!("keyboard_hook 已重装");
                        } else {
                            log::error!("keyboard_hook 重装失败");
                        }
                        continue;
                    }
                    TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }

                UnhookWindowsHookEx(hook);
                // 代际竞态防护：慢退出的旧线程若无条件清零静态槽，会把新线程
                // 刚注册的 tid/hook 抹掉——下一次 stop() 变 no-op，writer join
                // 永久挂死。只清自己仍然持有的槽位。
                let _ =
                    KB_HOOK.compare_exchange(hook as u32, 0, Ordering::AcqRel, Ordering::Acquire);
                let _ =
                    KB_THREAD_ID.compare_exchange(my_tid, 0, Ordering::AcqRel, Ordering::Acquire);
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
        // P0 关停挂死修复：仅 PostThreadMessageW(WM_QUIT) 只结束 hook 线程，
        // 静态槽里的 Sender 若不 drop，channel 永不 Disconnected，collector
        // writer 的 join 会永久阻塞（writer 只认通道断开）。取出并 drop。
        // 顺序安全：先 Post 再 take，回调里短命 clone 的发送走 send_event 的
        // Disconnected 分支，不会报错。
        let _ = take_kb_tx();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// stop 的核心语义：静态槽位中的 Sender 被 take 走并 drop 后，通道必须
    /// 进入 Disconnected（recv 返回断开、send 返回错误）。用注入的 channel
    /// 直接驱动 take_kb_tx 的生命周期，不依赖真实 Win32 hook。
    #[test]
    fn stop_takes_sender_and_disconnects_channel() {
        let (tx, rx) = crossbeam_channel::bounded::<Event>(1);
        if let Ok(mut g) = KB_TX.lock() {
            *g = Some(tx.clone());
        }

        // 模拟 stop() 的取回动作
        let taken = take_kb_tx();
        assert!(taken.is_some(), "stop 必须能取回槽位中的 Sender");
        assert!(
            KB_TX.lock().unwrap().is_none(),
            "stop 后静态槽位必须为空（可重启语义不变）"
        );

        // 所有 Sender clone 全部 drop 后，接收端必须看到 Disconnected
        drop(taken);
        drop(tx);
        assert!(
            matches!(
                rx.try_recv(),
                Err(crossbeam_channel::TryRecvError::Disconnected)
            ),
            "sender drop 后通道必须 Disconnected，否则 writer join 永久阻塞"
        );
    }
}
