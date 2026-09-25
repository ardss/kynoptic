//! 会话状态监控（锁定/解锁/会话切换/显示器变化）
//!
//! 双通路设计（重构）：
//! - 主通路：专用消息线程建隐藏顶层窗口并注册 WTS 会话通知
//!   （WTSRegisterSessionNotification + WM_WTSSESSION_CHANGE），锁/解锁、
//!   控制台/RDP 连入断开均实时精确收到，事件在发生时刻构造（时间戳保真），
//!   排入 [`WTS_PENDING`]，由本监控器 collect 时排空转发。同一窗口还接收
//!   WM_DISPLAYCHANGE，显示器拓扑变化（含主屏切换、同数量换屏）即时捕获。
//!   API 形状已由真机探针验证（register/unregister 返回非零、消息路由正确）。
//! - 兜底：OpenInputDesktop 5s 轮询保留。WTS 注册失败（如非交互会话）时
//!   维持原有 2 拍去抖行为；WTS 生效时轮询只做"解锁恢复"（WTS 报锁定但
//!   输入桌面可打开 → 补发 Unlock），**不再由轮询发 Lock**——UAC 安全桌面
//!   同样使 OpenInputDesktop 失败，轮询发 Lock 会重引 UAC 误报，而 WTS
//!   SESSION_LOCK 是系统推给的确定事实，不存在漏发即漏记的路径。

use crate::types::*;
use serde_json::json;
use std::cell::Cell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Duration;
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

/// WTS 通知线程待发事件队列（SessionMonitor::collect 排空转发）。
/// 事件构造发生在通知到达时刻，collect 只搬运，时间戳不失真。
static WTS_PENDING: Mutex<Vec<Event>> = Mutex::new(Vec::new());
/// 精确锁屏态：WTS SESSION_LOCK/UNLOCK 与轮询兜底共同维护。
/// keyboard_hook watchdog 据此在锁屏期间暂停摘钩判定（见 keyboard_hook.rs）。
static SESSION_LOCKED: AtomicBool = AtomicBool::new(false);
/// WTS 通知注册成功（主通路生效）
static WTS_ACTIVE: AtomicBool = AtomicBool::new(false);
/// 通知线程只启一次
static WTS_STARTED: AtomicBool = AtomicBool::new(false);

/// 当前是否处于锁定态（供其他监控器查询，如 keyboard_hook watchdog）。
pub fn session_locked() -> bool {
    SESSION_LOCKED.load(Ordering::Relaxed)
}

// ── WTS 常量与 FFI（core 未启用 Win32_System_RemoteDesktop feature，
//    按本模块既有风格 extern 声明；形状经真机探针验证）──
const WM_WTSSESSION_CHANGE: u32 = 0x02B1;
const WTS_CONSOLE_CONNECT: usize = 1;
const WTS_CONSOLE_DISCONNECT: usize = 2;
const WTS_REMOTE_CONNECT: usize = 3;
const WTS_REMOTE_DISCONNECT: usize = 4;
const WTS_SESSION_LOCK: usize = 7;
const WTS_SESSION_UNLOCK: usize = 8;
const NOTIFY_FOR_THIS_SESSION: u32 = 0;
const WM_DISPLAYCHANGE: u32 = 0x007E;
/// 通知窗口类名：进程内私有 atom（未加 CS_GLOBALCLASS），非内核命名对象
const WTS_CLASS: &str = "KynopticSessionWts";

extern "system" {
    fn WTSRegisterSessionNotification(hwnd: HWND, flags: u32) -> i32;
    fn WTSUnRegisterSessionNotification(hwnd: HWND) -> i32;
    fn GetModuleHandleW(name: *const u16) -> HANDLE;
}

/// 队列一条事件（通知线程 → collect）
fn enqueue(ev: Event) {
    if let Ok(mut q) = WTS_PENDING.lock() {
        q.push(ev);
    }
}

fn queue_lock_change(locked: bool, source: &str) {
    SESSION_LOCKED.store(locked, Ordering::Release);
    enqueue(
        Event::new(
            if locked {
                EventAction::Lock
            } else {
                EventAction::Unlock
            },
            EventType::Session,
        )
        .data(json!({ "locked": locked, "source": source })),
    );
}

/// WM_WTSSESSION_CHANGE 分发：wparam → 事件
fn on_wts_change(wparam: usize) {
    match wparam {
        WTS_SESSION_LOCK => queue_lock_change(true, "wts_session_lock"),
        WTS_SESSION_UNLOCK => queue_lock_change(false, "wts_session_unlock"),
        // 会话切换（快速用户切换 / RDP 连入断开）：不属于锁定，只记切换事实
        WTS_CONSOLE_CONNECT => enqueue(Event::new(EventAction::Switch, EventType::Session).data(
            json!({
                "switch": "console_connect", "source": "wts",
            }),
        )),
        WTS_CONSOLE_DISCONNECT => enqueue(
            Event::new(EventAction::Switch, EventType::Session).data(json!({
                "switch": "console_disconnect", "source": "wts",
            })),
        ),
        WTS_REMOTE_CONNECT => enqueue(Event::new(EventAction::Switch, EventType::Session).data(
            json!({
                "switch": "remote_connect", "source": "wts",
            }),
        )),
        WTS_REMOTE_DISCONNECT => enqueue(Event::new(EventAction::Switch, EventType::Session).data(
            json!({
                "switch": "remote_disconnect", "source": "wts",
            }),
        )),
        _ => {} // SESSION_LOGON/LOGOFF/REMOTE_CONTROL 等不在采集口径内
    }
}

/// 枚举各显示器：分辨率 + 主屏标志（WM_DISPLAYCHANGE 事件数据）
fn monitor_snapshot() -> Vec<serde_json::Value> {
    let mut list: Vec<serde_json::Value> = Vec::new();
    unsafe {
        let cb: MONITORENUMPROC = Some(mon_enum);
        EnumDisplayMonitors(
            std::ptr::null_mut(),
            std::ptr::null(),
            cb,
            &mut list as *mut _ as isize,
        );
    }
    list
}

unsafe extern "system" fn mon_enum(
    hmonitor: HMONITOR,
    _dc: HDC,
    _rect: *mut RECT,
    lparam: LPARAM,
) -> BOOL {
    let list = &mut *(lparam as *mut Vec<serde_json::Value>);
    let mut info: MONITORINFOEXW = std::mem::zeroed();
    info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
    if GetMonitorInfoW(hmonitor, &mut info.monitorInfo) != 0 {
        let r = info.monitorInfo.rcMonitor;
        list.push(json!({
            // MonitorFromWindow 主屏判定等价口径：MONITORINFOF_PRIMARY
            "primary": (info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY) != 0,
            "width": r.right - r.left,
            "height": r.bottom - r.top,
        }));
    }
    1 // 继续枚举
}

/// WTS/广播通知线程主体：隐藏顶层窗口 + 消息循环，进程生命周期常驻。
/// （Monitor 无 stop 钩子，线程不随 collector 关停；托盘/CLI 进程退出即消亡，
/// 与 keyboard_hook 的常驻监视线程同口径。）
fn wts_window_thread() {
    unsafe {
        let hinstance = GetModuleHandleW(std::ptr::null());
        let cls: Vec<u16> = format!("{WTS_CLASS}\0").encode_utf16().collect();
        let wc = WNDCLASSW {
            style: 0,
            lpfnWndProc: Some(wts_wndproc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinstance,
            hIcon: std::ptr::null_mut(),
            hCursor: std::ptr::null_mut(),
            hbrBackground: std::ptr::null_mut(),
            lpszMenuName: std::ptr::null(),
            lpszClassName: cls.as_ptr(),
        };
        if RegisterClassW(&wc) == 0 {
            log::warn!("session 通知窗口注册失败，退化为纯轮询兜底");
            return;
        }
        let title: Vec<u16> = "Kynoptic session notify\0".encode_utf16().collect();
        let hwnd = CreateWindowExW(
            0,
            cls.as_ptr(),
            title.as_ptr(),
            WS_OVERLAPPED, // 不可见零尺寸顶层窗口
            0,
            0,
            0,
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            hinstance,
            std::ptr::null_mut(),
        );
        if hwnd.is_null() {
            log::warn!("session 通知窗口创建失败，退化为纯轮询兜底");
            return;
        }
        if WTSRegisterSessionNotification(hwnd, NOTIFY_FOR_THIS_SESSION) != 0 {
            WTS_ACTIVE.store(true, Ordering::Release);
            log::info!("session: WTS 会话通知已注册（锁/解锁/会话切换走主通路）");
        } else {
            log::warn!("session: WTSRegisterSessionNotification 失败，退化为纯轮询兜底");
        }
        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        let _ = DestroyWindow(hwnd);
    }
}

unsafe extern "system" fn wts_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_WTSSESSION_CHANGE => {
            on_wts_change(wparam);
            0
        }
        WM_DISPLAYCHANGE => {
            // 显示器拓扑变化：新色深(wparam)、新分辨率(LOWORD/HIWORD lparam)
            let monitors = monitor_snapshot();
            enqueue(
                Event::new(EventAction::DisplayChange, EventType::Session).data(json!({
                    "source": "displaychange",
                    "bpp": wparam as u32,
                    "width": (lparam & 0xFFFF) as i32,
                    "height": ((lparam >> 16) & 0xFFFF) as i32,
                    "monitors": monitors,
                })),
            );
            0
        }
        WM_DESTROY => {
            if WTS_ACTIVE.load(Ordering::Acquire) {
                WTSUnRegisterSessionNotification(hwnd);
            }
            PostQuitMessage(0);
            0
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

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
        // 首次采集时拉起通知线程（只一次；成败不影响轮询兜底）
        if !WTS_STARTED.swap(true, Ordering::AcqRel) {
            let _ = std::thread::Builder::new()
                .name("session_wts".into())
                .spawn(wts_window_thread);
        }

        // 主通路：搬运 WTS/WM_DISPLAYCHANGE 队列（时间戳为通知到达时刻）
        if let Ok(mut q) = WTS_PENDING.lock() {
            for ev in q.drain(..) {
                let _ = tx.try_send(ev);
            }
        }

        // 兜底轮询：OpenInputDesktop 在锁定时返回 NULL。Win32 语义：
        // 输入桌面在锁屏/UAC 安全桌面激活期间不开放——因此 UAC 同意弹窗
        // 也会得到 NULL，这正是 WTS 主通路要消除的误报源。
        let is_locked = unsafe {
            let desktop = OpenInputDesktop(0, 0, 0x0001); // DESKTOP_READOBJECTS
            if !desktop.is_null() {
                CloseDesktop(desktop);
                false
            } else {
                true
            }
        };

        // 去抖计数照常累计（纯轮询模式沿用；WTS 模式下仅作观测）
        if is_locked {
            self.locked_streak.set(self.locked_streak.get() + 1);
            self.unlocked_streak.set(0);
        } else {
            self.unlocked_streak.set(self.unlocked_streak.get() + 1);
            self.locked_streak.set(0);
        }

        let prev_locked = self.was_locked.get();
        if WTS_ACTIVE.load(Ordering::Acquire) {
            // 主通路生效：轮询仅兜底"解锁恢复"（WTS 解锁事件丢失或未送达时，
            // 桌面已可打开即补发，1 拍即可——解锁方向没有 UAC 形态的干扰源）。
            // 反方向（轮询报锁、WTS 报解锁）= UAC 安全桌面特征，忽略不发 Lock。
            if SESSION_LOCKED.load(Ordering::Acquire) && !is_locked {
                SESSION_LOCKED.store(false, Ordering::Release);
                let event = Event::new(EventAction::Unlock, EventType::Session)
                    .data(json!({ "locked": false, "source": "poll_recovery" }));
                let _ = tx.try_send(event);
            }
            self.was_locked.set(SESSION_LOCKED.load(Ordering::Relaxed));
        } else {
            // 纯轮询模式（WTS 注册失败）：维持原有 2 拍去抖行为不变。
            // 单拍 NULL 不足以翻转状态——UAC 弹窗往往只存在一两个轮询周期；
            // 代价是 <10s 的真实锁/解可能整段漏记（主通路生效时该缺陷消除）。
            if !prev_locked && self.locked_streak.get() >= 2 {
                self.was_locked.set(true);
                SESSION_LOCKED.store(true, Ordering::Release);
                let event = Event::new(EventAction::Lock, EventType::Session)
                    .data(json!({ "locked": true, "source": "poll" }));
                let _ = tx.try_send(event);
            } else if prev_locked && self.unlocked_streak.get() >= 2 {
                self.was_locked.set(false);
                SESSION_LOCKED.store(false, Ordering::Release);
                let event = Event::new(EventAction::Unlock, EventType::Session)
                    .data(json!({ "locked": false, "source": "poll" }));
                let _ = tx.try_send(event);
            }
        }

        // 兜底：显示器数量变化（数量比较保留；分辨率/主屏变化由
        // WM_DISPLAYCHANGE 主通路覆盖，这里只补数量维度的漏检）
        let monitor_count = unsafe { GetSystemMetrics(SM_CMONITORS) };
        let prev_monitors = self.last_monitors.get();
        if prev_monitors > 0 && monitor_count != prev_monitors {
            let event = Event::new(EventAction::DisplayChange, EventType::Session).data(json!({
                "source": "poll",
                "prev_count": prev_monitors,
                "current_count": monitor_count,
            }));
            let _ = tx.try_send(event);
        }
        self.last_monitors.set(monitor_count);
    }
}

use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::Graphics::Gdi::{EnumDisplayMonitors, GetMonitorInfoW, MONITORINFOEXW};

extern "system" {
    fn OpenInputDesktop(dwFlags: u32, fInherit: BOOL, dwDesiredAccess: u32) -> HANDLE;
    fn CloseDesktop(hDesktop: HANDLE) -> BOOL;
}

const SM_CMONITORS: i32 = 80;

#[cfg(test)]
mod tests {
    use super::*;

    /// 锁屏态查询与事件队列的推演钉子：queue_lock_change 必须同步翻转
    /// SESSION_LOCKED 并把带 source 标注的事件入队（keyboard_hook watchdog
    /// 与 collect 排空逻辑共同依赖该语义）。
    #[test]
    fn wts_lock_change_flips_state_and_enqueues() {
        let before = session_locked();
        queue_lock_change(!before, "wts_session_lock");
        assert_eq!(session_locked(), !before);
        {
            let mut q = WTS_PENDING.lock().unwrap();
            let ev = q.pop().expect("必须入队一条事件");
            let d = ev.event_data.expect("必须带 data");
            assert_eq!(d["source"], "wts_session_lock");
            assert_eq!(d["locked"], !before);
            // 只取自己这条，恢复队列原状
            q.clear();
        }
        queue_lock_change(before, "wts_session_unlock");
        assert_eq!(session_locked(), before);
        WTS_PENDING.lock().unwrap().clear();
    }

    /// 显示器快照：真机枚举至少 1 块屏，字段齐全且恰有一块主屏。
    #[test]
    fn monitor_snapshot_shape() {
        let snap = monitor_snapshot();
        assert!(!snap.is_empty(), "至少应枚举到一块显示器");
        assert_eq!(snap.iter().filter(|m| m["primary"] == true).count(), 1);
        for m in &snap {
            assert!(m["width"].as_i64().unwrap_or(0) > 0);
            assert!(m["height"].as_i64().unwrap_or(0) > 0);
        }
    }
}
