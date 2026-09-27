//! 前台窗口切换监控
//!
//! 双通道（发现 platform high 重构）：
//! 1. 轮询通道：collector 按 interval() 驱动 collect()，兜底 + 低配模式主力；
//! 2. 事件通道：SetWinEventHook(EVENT_SYSTEM_FOREGROUND) 出上下文钩子，
//!    前台切换即时捕获（探针实测安装成功；探针限制下投递未在本机实测，
//!    轮询通道兜底保证正确性）。
//!
//! 两通道共享全局 last-emitted 去重（前台窗口全机唯一，跨通道去重必须
//! 全局而非 per-instance）与 Sender 静态槽（collector shutdown 时
//! stop_event_channel 取走并 drop，通道可断开，writer join 不挂死——
//! 与 keyboard_hook 的 KB_TX 同款 P0 口径；事件线程本体空闲时阻塞在
//! GetMessageW，零 CPU）。

use crate::types::*;
use crossbeam_channel::Sender;
use serde_json::json;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

// Win32_UI_Accessibility 未进依赖表（keyboard_hook 先例：手写 extern 声明
// 避免动 Cargo features），签名与 WinUser.h 一致（探针实测安装成功）。
extern "system" {
    fn SetWinEventHook(
        event_low: u32,
        event_high: u32,
        h_mod: *mut c_void,
        p_vnd: Option<
            unsafe extern "system" fn(
                _: *mut c_void,
                _: u32,
                _: HWND,
                _: u32,
                _: u32,
                _: u32,
                _: u32,
            ),
        >,
        id_proc: u32,
        id_thread: u32,
        dw_flags: u32,
    ) -> *mut c_void;
    fn UnhookWinEvent(h_hook: *mut c_void) -> i32;
}
use std::os::raw::c_void;
const EVENT_SYSTEM_FOREGROUND: u32 = 3;
const WINEVENT_OUTOFCONTEXT: u32 = 0;
const WINEVENT_SKIPOWNPROCESS: u32 = 0x0002;
const WM_QUIT: u32 = 0x0012;

/// 最近一次已发出的前台窗口 hwnd（全机唯一前台窗，全局去重）。
static FG_LAST_EMITTED: AtomicU64 = AtomicU64::new(0);
/// 事件通道的 Sender 槽（keyboard_hook KB_TX 同款语义：每拍刷新 = 活跃
/// collector；关停时 take 走并 drop → 通道可断开，writer join 不挂死。
/// 事件线程本体进程级常驻（无 Sender 时回调空转，零 CPU）。
static FG_TX: Mutex<Option<Sender<Event>>> = Mutex::new(None);
/// 事件线程 tid（0 = 线程已退出/未启动；重启时重新存）。
static FG_THREAD_ID: AtomicU32 = AtomicU32::new(0);
/// 事件线程启动标志（进程级一次；装钩失败不重试，轮询兜底）。
static FG_THREAD_STARTED: AtomicBool = AtomicBool::new(false);

/// 本进程 exe 文件名（小写，如 "kynoptic.exe" / "kynoptic-tray.exe"），
/// 进程生命周期内不变，缓存一次。取不到返回 None（不排除任何东西）。
fn self_exe_name() -> Option<String> {
    use std::sync::OnceLock;
    static CACHE: OnceLock<Option<String>> = OnceLock::new();
    CACHE
        .get_or_init(|| {
            std::env::current_exe()
                .ok()
                .map(|p| normalize_exe_name(&p.to_string_lossy()))
        })
        .clone()
}

/// 规范化 exe 名用于比较：取文件名、统一小写、统一斜杠。
/// 纯函数，单测覆盖（不依赖真实进程）。
fn normalize_exe_name(path: &str) -> String {
    let name = path.rsplit(['\\', '/']).next().unwrap_or(path);
    name.trim_matches('"').to_ascii_lowercase()
}

/// 是否为本采集器进程自身的 exe（kynoptic.exe / kynoptic-tray.exe 等——
/// 本 crate 编译出的任一 bin 都会被比对命中）。纯函数，单测覆盖。
///
/// 排除原因：采集器/托盘自己也会成为前台窗口（托盘弹菜单、以窗口形式
/// 短暂出现的辅助进程），它们产生的 window/switch 事件会以 "kynoptic.exe"
/// 污染 Top 应用排行，首装空库时甚至让 TOP 应用只有采集器 owner 自己。
/// 数据口径：我们度量的是"用户在用电脑干什么"，采集器自身不属于该口径。
fn exe_matches_self(self_exe: &str, other_proc: &str) -> bool {
    if self_exe.is_empty() || other_proc.is_empty() {
        return false;
    }
    normalize_exe_name(other_proc) == normalize_exe_name(self_exe)
}

pub struct WindowMonitor;

impl Default for WindowMonitor {
    fn default() -> Self {
        Self
    }
}

impl Monitor for WindowMonitor {
    fn name(&self) -> &str {
        "window"
    }

    /// 双通道采样口径：事件通道即时捕获前台切换；轮询通道 200ms 兜底
    /// （事件通道装失败/降级时仍有采样）。低配模式（发现 perf medium）：
    /// 轮询放档到 1s，CPU 摊薄一档。
    fn interval(&self) -> Duration {
        if super::low_power_mode() {
            Duration::from_secs(1)
        } else {
            Duration::from_millis(200)
        }
    }

    fn collect(&self, tx: &crossbeam_channel::Sender<Event>) {
        // 刷新事件通道 Sender 槽（最后一拍写入者 = 活跃 collector）
        if let Ok(mut g) = FG_TX.lock() {
            *g = Some(tx.clone());
        }
        ensure_fg_event_channel();
        capture_foreground();
    }
}

/// 启动事件通道线程（进程级一次）。钩子安装失败仅降级（轮询兜底），
/// 计数进 MONITOR_DEGRADED。
fn ensure_fg_event_channel() {
    if FG_THREAD_STARTED.swap(true, Ordering::AcqRel) {
        return;
    }
    let ok = std::thread::Builder::new()
        .name("window_foreground_events".into())
        .spawn(fg_event_thread)
        .is_ok();
    if !ok {
        log::warn!("window: 前台事件线程启动失败，仅轮询通道（低配兜底）");
    }
}

/// 事件通道线程体：降优先级 → 装钩子 → 存 tid → 消息循环（出上下文事件
/// 经本线程消息队列投递，回调在 DispatchMessageW 时执行）。
fn fg_event_thread() {
    use windows_sys::Win32::System::Threading::{
        GetCurrentThread, GetCurrentThreadId, SetThreadPriority, THREAD_PRIORITY_BELOW_NORMAL,
    };
    unsafe {
        // 采集线程降优先级（发现 perf medium）：钩子消息循环长期空转，
        // BELOW_NORMAL 避免抢交互线程。
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL);

        let hook = SetWinEventHook(
            EVENT_SYSTEM_FOREGROUND,
            EVENT_SYSTEM_FOREGROUND,
            std::ptr::null_mut(),
            Some(fg_event_proc),
            0,
            0,
            WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
        );
        let my_tid = GetCurrentThreadId();
        if hook.is_null() {
            log::warn!("window: 前台事件钩子安装失败，仅轮询通道（低配兜底）");
            super::note_degraded("window: 前台事件钩子安装失败");
            FG_THREAD_ID.store(0, Ordering::Release);
            return;
        }
        FG_THREAD_ID.store(my_tid, Ordering::Release);
        log::info!("window: 前台事件通道已安装（轮询兜底）");

        let mut msg: MSG = std::mem::zeroed();
        loop {
            let r = GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0);
            if r <= 0 {
                if r == -1 {
                    // GetMessageW 出错返回 -1：睡眠后再试（keyboard_hook 同款）
                    std::thread::sleep(Duration::from_millis(50));
                    continue;
                }
                break; // WM_QUIT（stop_event_channel 投递）
            }
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        let _ = UnhookWinEvent(hook);
        // 代际竞态防护（keyboard_hook 同款）：只清自己仍持有的 tid 槽
        let _ = FG_THREAD_ID.compare_exchange(my_tid, 0, Ordering::AcqRel, Ordering::Acquire);
        log::info!("window: 前台事件通道已停止");
    }
}

/// 事件通道回调（在事件线程消息循环中执行）：转到共享捕获路径。
unsafe extern "system" fn fg_event_proc(
    _hook: *mut c_void,
    _event: u32,
    _hwnd: HWND,
    _id_obj: u32,
    _id_child: u32,
    _thread: u32,
    _time: u32,
) {
    capture_foreground();
}

/// 关停事件通道（collector shutdown 调用）：先 Post WM_QUIT 结束消息循环，
/// 再 take 走 Sender 并 drop（keyboard_hook stop 同款顺序）——静态槽里的
/// Sender 不 drop，通道永不 Disconnected，collector writer join 会永久
/// 阻塞。同时解除 FG_THREAD_STARTED，下一代 collector 首拍可重开事件线程
/// （旧线程退出时靠 FG_THREAD_ID CAS 自清理，双钩子短暂并存时全局去重
/// 保证不双发）。
pub fn stop_event_channel() {
    let tid = FG_THREAD_ID.load(Ordering::Acquire);
    if tid != 0 {
        unsafe {
            PostThreadMessageW(tid, WM_QUIT, 0, 0);
        }
    }
    if let Ok(mut g) = FG_TX.lock() {
        let _ = g.take();
    }
    // 允许下一代 collector 首拍重开事件线程（代际竞态无害：旧线程靠
    // FG_THREAD_ID 的 CAS 自清理，双钩子短暂并存时全局去重保证不双发）
    FG_THREAD_STARTED.store(false, Ordering::Release);
}

/// 共享前台捕获路径（轮询与事件通道同走）：全局去重 → 读标题/pid →
/// 宿主感知解析（发现 platform high）→ 自身排除 → 落事件。
fn capture_foreground() {
    // 克隆 Sender 后立即释放锁（keyboard_hook 同款，避免回调内长时间持锁）。
    // 槽为空（collector 未启动或已 stop）→ 无事件可发。
    let Some(tx) = FG_TX.lock().ok().and_then(|g| g.clone()) else {
        return;
    };

    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.is_null() {
            return;
        }
        let key = hwnd as u64;
        // 全局去重（无锁 CAS 重试）：同一前台窗已发过即跳过；A→B→A
        // 切回时 last≠A 重新发出。双通道并发时恰一者胜出。
        loop {
            let last = FG_LAST_EMITTED.load(Ordering::Acquire);
            if last == key {
                return;
            }
            if FG_LAST_EMITTED
                .compare_exchange(last, key, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                break;
            }
        }

        // 获取窗口标题
        let mut buf = [0u16; 512];
        let len = GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32);
        let title = String::from_utf16_lossy(&buf[..len as usize]);

        // 获取进程 ID 与进程名（app_name 铁律：不得留空）
        let mut pid: u32 = 0;
        GetWindowThreadProcessId(hwnd, &mut pid);
        let proc_name = super::browser::get_process_name(pid);

        // 自身进程排除（宿主感知解析之前，用属主原始名）：last 已更新
        // （避免重复窗口再比对），但不产生事件——见 exe_matches_self 注释。
        if self_exe_name()
            .map(|s| exe_matches_self(&s, &proc_name))
            .unwrap_or(false)
        {
            log::debug!("window: skip self process ({proc_name})");
            return;
        }

        // 宿主感知解析（发现 platform high）：属主是 UWP/内置 XAML/控制台
        // 宿主时还原真实应用名；解析失败保留宿主名（展示层归一友好标签）。
        let (app_name, remote) = host_aware_app_name(pid, &proc_name, hwnd, &title);

        log::debug!("window: foreground changed to {key:x} ({app_name})");

        // 标题脱敏（opt-in，见 title_privacy）：默认原样，开启后剥 URL 查询串
        let title = super::title_privacy::redact_title(&title);

        let mut data = json!({
            "hwnd": key,
            "title": title,
            "pid": pid,
            "proc": proc_name,
        });
        // VM/RDP/远控宿主标注（发现 platform medium）：真实活动发生在
        // 客户机/远端，展示侧配「远程/虚拟机」分类
        if remote {
            data["remote_session"] = json!(true);
        }

        let event = Event::new(EventAction::Switch, EventType::Window)
            .data(data)
            .app(&app_name, &title);

        crate::collector::send_event(&tx, event);
    }
}

/// 宿主感知解析：属主基名是宿主时还原真实应用名，返回 (app_name, 是否远控宿主)。
/// 解析失败 → 保留属主原始名（不造空/哨兵值，展示层归一兜底）。
fn host_aware_app_name(pid: u32, raw_name: &str, hwnd: HWND, title: &str) -> (String, bool) {
    use super::host;
    let base = host::base_of(raw_name);
    let remote = host::is_remote_host(&base);
    if !host::is_window_host(&base) {
        return (raw_name.to_string(), remote);
    }
    let full = super::browser::process_full_path(pid);
    let app = host::resolve_host_app(hwnd, pid, &base, full.as_deref(), title)
        .unwrap_or_else(|| raw_name.to_string());
    (app, remote)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_exe_name_handles_slashes_quotes_case() {
        assert_eq!(
            normalize_exe_name(r"C:\apps\Kynoptic\kynoptic.exe"),
            "kynoptic.exe"
        );
        assert_eq!(normalize_exe_name("kynoptic-tray.exe"), "kynoptic-tray.exe");
        assert_eq!(normalize_exe_name("/usr/bin/KYNOPTIC"), "kynoptic");
        assert_eq!(normalize_exe_name(r#""C:\x\kynoptic.exe""#), "kynoptic.exe");
    }

    #[test]
    fn self_process_excluded_for_all_own_bins() {
        // 本 crate 编译出的任一 bin（采集器/托盘/ctl）在作为 current_exe 时
        // 都不该进前台事件；且每个 bin 只匹配自己的名字，不误伤兄弟 bin。
        for own in ["kynoptic.exe", "kynoptic-tray.exe", "kynoptic-ctl.exe"] {
            assert!(exe_matches_self(own, own), "{own} 应与自身匹配");
            for other in ["kynoptic.exe", "kynoptic-tray.exe", "kynoptic-ctl.exe"] {
                if other != own {
                    assert!(
                        !exe_matches_self(other, own),
                        "{own} 不应误伤兄弟进程 {other}"
                    );
                }
            }
        }
        // 大小写与引号路径归一化后仍应命中自身（kynoptic.exe 归一化即此名）
        assert!(exe_matches_self(
            "kynoptic.exe",
            r"C:\Program Files\Kynoptic\KYNOPTIC.EXE"
        ));
        assert!(exe_matches_self("kynoptic.exe", r#""C:\x\kynoptic.exe""#));
    }

    #[test]
    fn foreign_processes_never_excluded() {
        assert!(!exe_matches_self("kynoptic.exe", "chrome.exe"));
        assert!(!exe_matches_self("kynoptic.exe", "kynoptic-viewer.exe"));
        assert!(!exe_matches_self("kynoptic.exe", "notkynoptic.exe"));
    }

    #[test]
    fn empty_names_are_never_self() {
        // proc 名取不到（OpenProcess 失败返回空串）时不得连带排除真实事件
        assert!(!exe_matches_self("kynoptic.exe", ""));
        assert!(!exe_matches_self("", "kynoptic.exe"));
    }
}
