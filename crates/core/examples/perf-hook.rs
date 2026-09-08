//! perf-hook — 输入 Hook 回调成本基准（HOOK COST bench，best effort）
//!
//! 低级键盘/鼠标 Hook 回调不在进程内可独立计时（需真实消息循环），故按
//! "回调体逻辑等价物"计时：与 keyboard_proc/mouse_proc 相同的每事件工作
//! —— Mutex<Option<Sender>> 取出克隆、GetAsyncKeyState 修饰键采样、
//! serde_json::json! 构造、crossbeam try_send —— 以百万次迭代测 ns/call，
//! 并给出纯计数器增量（AtomicU64::fetch_add）作下限对照。
//!
//! 真实回调额外开销只有函数调用 + CallNextHookEx（用户态跳转），可忽略；
//! 结论若 <1µs/call，则对输入延迟无可感知影响（人感知阈值 ~10ms）。
//!
//! 运行：`cargo run --release -p kynoptic-core --example perf-hook`

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Instant;

use crossbeam_channel::bounded;
use kynoptic_core::types::{Event, EventAction, EventType};

const ITERS: u64 = 1_000_000;

fn now_ns() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64
}

fn main() {
    let (tx, rx) = bounded::<Event>(20_000); // 与生产 CHANNEL_CAPACITY 一致
    let hook_tx: Mutex<Option<crossbeam_channel::Sender<Event>>> = Mutex::new(Some(tx));

    // 下限对照：纯计数器
    let counter = AtomicU64::new(0);
    let t0 = Instant::now();
    for _ in 0..ITERS {
        counter.fetch_add(1, Ordering::Relaxed);
    }
    let ns_counter = t0.elapsed().as_nanos() as f64 / ITERS as f64;
    std::hint::black_box(counter.load(Ordering::Relaxed));

    // 键盘回调等价物：锁 + GetAsyncKeyState x4 + json! + try_send
    #[allow(clippy::never_loop)]
    let t0 = Instant::now();
    let mut drained = 0u64;
    for i in 0..ITERS {
        let t = hook_tx.lock().ok().and_then(|g| g.clone());
        if let Some(tx) = t {
            #[cfg(windows)]
            let (shift, ctrl, alt, win) = unsafe {
                (
                    windows_sys::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState(0x10) < 0,
                    windows_sys::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState(0x11) < 0,
                    windows_sys::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState(0x12) < 0,
                    windows_sys::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState(0x5B) < 0,
                )
            };
            #[cfg(not(windows))]
            let (shift, ctrl, alt, win) = (false, false, false, false);

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
            let event =
                Event::new(EventAction::Press, EventType::Keyboard).data(serde_json::json!({
                    "key": "a",
                    "modifiers": modifiers,
                    "ts_hint": i,
                }));
            let _ = tx.try_send(event);
        }
        // 同步排空，避免通道打满后 try_send 变成纯 drop（与真实 writer 排水一致）
        if i % 64 == 63 {
            while let Ok(_e) = rx.try_recv() {
                drained += 1;
            }
        }
    }
    let ns_kb = t0.elapsed().as_nanos() as f64 / ITERS as f64;

    // 鼠标移动回调等价物：SystemTime 采样 + 节流判断 + json + try_send
    let t0 = Instant::now();
    let mut last = 0u64;
    for _ in 0..ITERS {
        let now_ms = now_ns();
        if now_ms.saturating_sub(last) < 500 {
            continue; // 真实回调的节流早退路径（大多数移动事件走这里）
        }
        last = now_ms;
        let event = Event::new(EventAction::Move, EventType::Mouse).data(serde_json::json!({
            "x": 100, "y": 200,
        }));
        if let Some(tx) = hook_tx.lock().ok().and_then(|g| g.clone()) {
            let _ = tx.try_send(event);
        }
    }
    let ns_mouse_throttled = t0.elapsed().as_nanos() as f64 / ITERS as f64;

    println!(
        r#"{{"bench":"perf-hook","iters":{ITERS},
  "atomic_counter_baseline_ns_per_call":{ns_counter:.1},
  "keyboard_callback_equivalent_ns_per_call":{ns_kb:.1},
  "mouse_move_throttled_early_exit_ns_per_call":{ns_mouse_throttled:.1},
  "drained":{drained}}}"#
    );
}
