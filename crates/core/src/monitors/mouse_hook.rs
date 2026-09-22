//! 鼠标低级 Hook（SetWindowsHookExW WH_MOUSE_LL）
//!
//! raw 粒度（默认）：点击/滚轮/释放全部发送，移动每 500ms 采样一次。
//! minute 粒度（opt-in，见 collector::CollectorSettings）：回调退化为
//! 纯原子计数（input_agg::record_*），不做节流（原子操作无洪泛风险）。
//!
//! 记录口径（复核 low，本轮不改代码）：按 Win32 文档化语义，WH_MOUSE_LL
//! 不接收 WM_POINTER/触屏手势路径的输入——触屏/触控笔用户的双指滚动不产生
//! WM_MOUSEWHEEL，故整类滚动行为不在记录口径内，presence 的"滚轮=主动
//! 阅读"启发对触屏用户会偏低。补齐需另装 WM_POINTER 处理路径，非本轮
//! 最小改动范围；此注释固化该口径。

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
/// hook 线程自定义消息：重装 hook（摘钩自愈，Wave20 P0，同 keyboard_hook）
const WM_APP_REHOOK: u32 = 0x8106;

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
                    // LLMHF_INJECTED（0x1）：注入滚轮单独累计（审查 MEDIUM：
                    // 滚轮连点器/自动化滚动不得计入人在场）
                    let injected = ms.flags & 0x1 != 0;
                    crate::input_agg::record_scroll((delta.unsigned_abs() / 120) as u64, injected);
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
                    // 审查 MEDIUM：LLMHF_INJECTED（0x1）归一化落为 "injected"
                    // 布尔——presence 的 raw 分支据此把注入点击排除出人在场
                    let injected = ms.flags & 0x1 != 0;
                    let event = Event::new(action, EventType::Mouse).data(json!({
                        "button": button, "x": ms.pt.x, "y": ms.pt.y, "pressed": pressed,
                        "injected": injected,
                    }));
                    crate::collector::send_event(&tx, event);
                }
                WM_MOUSEWHEEL => {
                    let delta = (ms.mouse_data >> 16) as i16 as i32;
                    let direction = if delta > 0 { "up" } else { "down" };
                    let steps = delta.unsigned_abs() / 120;
                    // 审查 MEDIUM：注入滚轮同样归一化落 "injected" 布尔
                    let injected = ms.flags & 0x1 != 0;
                    let event = Event::new(EventAction::Scroll, EventType::Mouse).data(json!({
                        "x": ms.pt.x, "y": ms.pt.y,
                        "direction": direction, "steps": steps, "dy": delta,
                        "injected": injected,
                    }));
                    crate::collector::send_event(&tx, event);
                }
                _ => {}
            }
        }
    }
    CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam)
}

/// 取出静态槽位中的 Sender（取出即从槽位移走，drop 时通道断开）。
/// stop() 与测试共用：channel Disconnected 是 collector writer join 的唯一
/// 退出条件（见 collector::writer_loop），所以关停必须让 sender 真正 drop。
fn take_mouse_tx() -> Option<Sender<Event>> {
    MOUSE_TX.lock().ok().and_then(|mut g| g.take())
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
                let my_tid = windows_sys::Win32::System::Threading::GetCurrentThreadId();
                MOUSE_THREAD_ID.store(my_tid, Ordering::Release);

                let mut hook =
                    SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), std::ptr::null_mut(), 0);
                if hook.is_null() {
                    log::error!("鼠标 Hook 安装失败");
                    let _ = MOUSE_THREAD_ID.compare_exchange(
                        my_tid,
                        0,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    );
                    return;
                }
                MOUSE_HOOK.store(hook as u32, Ordering::Release);
                log::info!("mouse_hook 已启动 (事件驱动, 移动节流 500ms)");

                // Wave20 P0 摘钩自愈（同 keyboard_hook）：光标在动但 hook
                // 长时间无事件 → 投递重装消息。移动节流 ≤500ms，30s 无事件
                // 且光标在动基本可断定被系统摘钩。
                {
                    std::thread::Builder::new()
                        .name("mouse_hook_watch".into())
                        .spawn(move || {
                            extern "system" {
                                fn GetCursorPos(pt: *mut POINT) -> i32;
                            }
                            let mut prev = POINT { x: 0, y: 0 };
                            if GetCursorPos(&mut prev) == 0 {
                                prev = POINT { x: -1, y: -1 };
                            }
                            loop {
                                std::thread::sleep(std::time::Duration::from_secs(10));
                                if MOUSE_THREAD_ID.load(Ordering::Acquire) != my_tid {
                                    return;
                                }
                                let mut cur = POINT { x: 0, y: 0 };
                                if GetCursorPos(&mut cur) == 0 {
                                    continue;
                                }
                                let moved = cur.x != prev.x || cur.y != prev.y;
                                prev = cur;
                                let stale = LAST_MOVE_TIME.load(Ordering::Relaxed) != 0 && {
                                    let now_ms = std::time::SystemTime::now()
                                        .duration_since(std::time::UNIX_EPOCH)
                                        .map(|d| d.as_millis() as u64)
                                        .unwrap_or(0);
                                    now_ms.saturating_sub(LAST_MOVE_TIME.load(Ordering::Relaxed))
                                        > 30_000
                                };
                                if moved && stale {
                                    log::warn!("mouse_hook 疑似被系统摘除，尝试重装");
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
                            std::thread::sleep(std::time::Duration::from_millis(50));
                            continue;
                        }
                        break;
                    }
                    if msg.message == WM_APP_REHOOK {
                        UnhookWindowsHookEx(hook);
                        let h = SetWindowsHookExW(
                            WH_MOUSE_LL,
                            Some(mouse_proc),
                            std::ptr::null_mut(),
                            0,
                        );
                        if !h.is_null() {
                            hook = h;
                            MOUSE_HOOK.store(h as u32, Ordering::Release);
                            log::info!("mouse_hook 已重装");
                        } else {
                            log::error!("mouse_hook 重装失败");
                        }
                        continue;
                    }
                    TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }

                UnhookWindowsHookEx(hook);
                // 代际竞态防护（同 keyboard_hook）：只清自己仍持有的槽位。
                let _ = MOUSE_HOOK.compare_exchange(
                    hook as u32,
                    0,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                );
                let _ = MOUSE_THREAD_ID.compare_exchange(
                    my_tid,
                    0,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                );
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
        // P0 关停挂死修复：仅 PostThreadMessageW(WM_QUIT) 只结束 hook 线程，
        // 静态槽里的 Sender 若不 drop，channel 永不 Disconnected，collector
        // writer 的 join 会永久阻塞（writer 只认通道断开）。取出并 drop。
        // 顺序安全：先 Post 再 take，回调里短命 clone 的发送走 send_event 的
        // Disconnected 分支，不会报错。
        let _ = take_mouse_tx();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// stop 的核心语义：静态槽位中的 Sender 被 take 走并 drop 后，通道必须
    /// 进入 Disconnected（recv 返回断开、send 返回错误）。用注入的 channel
    /// 直接驱动 take_mouse_tx 的生命周期，不依赖真实 Win32 hook。
    #[test]
    fn stop_takes_sender_and_disconnects_channel() {
        let (tx, rx) = crossbeam_channel::bounded::<Event>(1);
        if let Ok(mut g) = MOUSE_TX.lock() {
            *g = Some(tx.clone());
        }

        // 模拟 stop() 的取回动作
        let taken = take_mouse_tx();
        assert!(taken.is_some(), "stop 必须能取回槽位中的 Sender");
        assert!(
            MOUSE_TX.lock().unwrap().is_none(),
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
