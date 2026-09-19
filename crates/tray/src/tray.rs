//! 托盘主体:隐藏窗口 + Shell_NotifyIconW + 右键菜单 + 消息循环(纯 Win32)。
//!
//! 无主窗口:仅一个不可见顶层窗口接收托盘回调消息。Collector 归"采集器
//! 属主线程"(main.rs)所有,本模块经 mpsc 命令通道控制,避免所有权跨线程。
//! 设计取舍:
//! - 不用 HWND_MESSAGE 消息-only 窗口(部分 shell 版本不向其投递托盘回调)
//! - TrackPopupMenu 前调 SetForegroundWindow,否则菜单不随点击他处消失(KB135788)
//! - 全程不弹通知气泡(NIF_INFO 永不使用)

#![allow(non_snake_case)]

use std::sync::mpsc::Sender;

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows_sys::Win32::UI::Shell::{
    ShellExecuteW, Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE,
    NIM_MODIFY, NOTIFYICONDATAW,
};
use windows_sys::Win32::UI::WindowsAndMessaging as win;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow,
    DispatchMessageW, GetCursorPos, GetMessageW, GetWindowLongPtrW, LoadCursorW, PostQuitMessage,
    RegisterClassW, SetForegroundWindow, SetTimer, SetWindowLongPtrW, ShowWindow, TrackPopupMenu,
    TranslateMessage, GWLP_USERDATA, TPM_BOTTOMALIGN, TPM_LEFTALIGN, TPM_RETURNCMD, WM_APP,
    WM_COMMAND, WM_DESTROY, WM_ENDSESSION, WM_QUERYENDSESSION, WM_RBUTTONUP, WNDCLASSW,
    WS_OVERLAPPED,
};

use crate::args::Args;
use crate::icons::TrayIcons;
use crate::state::{MenuId, TrayState};

/// 托盘回调消息基址(自定义消息避开系统区段)。
const WM_TRAYICON: u32 = WM_APP + 1;
/// Shell_NotifyIconW 实例 id(单图标,固定 1)。
const TRAY_ID: u32 = 1;
const MF_STRING: u32 = 0;
const MF_SEPARATOR: u32 = 0x0000_0800;
const SW_SHOWNORMAL: i32 = 1;
const SW_HIDE: i32 = 0;
const WM_CONTEXTMENU: u32 = 0x0205;
const WM_LBUTTONDBLCLK: u32 = 0x0203;

/// 发给采集器属主线程的命令(main.rs 定义线程,这里只约定协议)。
#[derive(Debug, Clone)]
pub enum CollectorCmd {
    /// 启动/重启采集（设置变更触发；Paused 状态下会被忽略）
    Start,
    /// 用户从托盘菜单恢复采集（Paused 状态下也生效）
    Resume,
    /// 暂停:置停机旗标并 join writer(Collector::shutdown)
    Pause,
    /// 退出:优雅关停后属主线程返回
    Quit,
}

/// 托盘运行上下文(经 GWLP_USERDATA 挂在隐藏窗口上)。
struct TrayCtx {
    icons: TrayIcons,
    cmd_tx: Sender<CollectorCmd>,
    state: TrayState,
    args: Args,
}

/// 自动更新检查结果文件（data\update-available.txt，内容=新版本号）。
/// 托盘后台线程每日检查写入；读不到/内容怪 = 无更新。
fn update_available_version(db: &std::path::Path) -> Option<String> {
    let dir = db.parent()?;
    let txt = std::fs::read_to_string(dir.join("update-available.txt")).ok()?;
    let v = txt.trim().trim_start_matches('v');
    if v.len() >= 3 && v.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        // 与当前版本比较（一致性审查 P2：手动装完新版后文件要等下一次
        // 24h 检查才更新，不比较会挂着过期提示）
        (version_gt(v, env!("CARGO_PKG_VERSION"))).then(|| v.to_string())
    } else {
        None
    }
}

/// 语义化版本比较：a > b 视为真（仅支持数字三段式；本项目的 tag 形态）
fn version_gt(a: &str, b: &str) -> bool {
    let parse = |v: &str| -> Vec<u64> {
        v.split('-')
            .next()
            .unwrap_or(v)
            .split('.')
            .map(|p| p.parse().unwrap_or(0))
            .collect()
    };
    let (pa, pb) = (parse(a), parse(b));
    for i in 0..3 {
        let x = pa.get(i).copied().unwrap_or(0);
        let y = pb.get(i).copied().unwrap_or(0);
        if x != y {
            return x > y;
        }
    }
    false
}

/// 实际 dashboard 端口：优先读 db 目录下 dashboard-port.txt（bind 成功才写）。
fn dashboard_port_actual(fallback: u16) -> u16 {
    let db = kynoptic_core::db::resolve_db_path();
    if let Some(dir) = db.parent() {
        if let Ok(txt) = std::fs::read_to_string(dir.join("dashboard-port.txt")) {
            if let Ok(p) = txt.trim().parse::<u16>() {
                return p;
            }
        }
    }
    fallback
}

impl TrayCtx {
    fn icon(&self) -> win::HICON {
        self.icons.for_state(self.state)
    }

    fn tip_text(&self) -> &'static str {
        match self.state {
            // 双语（装机审查：托盘是英文系统之外用户唯一常驻可见面）
            TrayState::Running => "Kynoptic: collecting / 采集中 · 右键菜单",
            TrayState::Paused => "Kynoptic: paused / 已暂停",
            TrayState::Error => "Kynoptic: error / 异常",
        }
    }

    /// NIM_MODIFY:换图标 + 换 tooltip(绝不弹气泡)。
    fn modify_icon(&self, hwnd: HWND) {
        let mut nid = tray_base(hwnd);
        nid.uFlags = NIF_ICON | NIF_TIP;
        nid.hIcon = self.icon();
        set_tip(&mut nid, self.tip_text());
        unsafe {
            Shell_NotifyIconW(NIM_MODIFY, &nid);
        }
    }

    fn set_state(&mut self, hwnd: HWND, state: TrayState) {
        self.state = state;
        self.modify_icon(hwnd);
    }

    /// 右键弹出菜单(五项+可选更新项)。
    fn show_menu(&mut self, hwnd: HWND) {
        unsafe {
            let menu = CreatePopupMenu();
            if menu.is_null() {
                return;
            }
            append_item(menu, MenuId::OpenDashboard, self.state);
            append_item(menu, MenuId::TogglePause, self.state);
            append_item(menu, MenuId::OpenDataFolder, self.state);
            append_item(menu, MenuId::About, self.state);
            // 自动更新检查发现新版本时插入一键更新项（日常检查由后台线程
            // 写 data\update-available.txt，见 main.rs）
            if let Some(ver) = update_available_version(&self.args.db) {
                // 文案单一来源（Wave17 P1：分安装形态，不做虚假 install 承诺）
                let installed = std::env::current_exe()
                    .ok()
                    .and_then(|e| e.parent().map(|p| p.to_path_buf()))
                    .map(|d| d.join("unins000.exe").exists())
                    .unwrap_or(false);
                let text: Vec<u16> = format!(
                    "{}\0",
                    crate::state::MenuId::update_menu_label(&ver, installed)
                )
                .encode_utf16()
                .collect();
                AppendMenuW(menu, MF_STRING, MenuId::UpdateNow as usize, text.as_ptr());
            }
            AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null());
            append_item(menu, MenuId::Quit, self.state);

            SetForegroundWindow(hwnd);
            let mut pt = POINT { x: 0, y: 0 };
            GetCursorPos(&mut pt);
            // TPM_RETURNCMD:选中项作为返回值,不再投递 WM_COMMAND
            let chosen = TrackPopupMenu(
                menu,
                TPM_LEFTALIGN | TPM_BOTTOMALIGN | TPM_RETURNCMD,
                pt.x,
                pt.y,
                0,
                hwnd,
                std::ptr::null_mut(),
            );
            DestroyMenu(menu);
            if chosen != 0 {
                self.handle_command(hwnd, chosen as u32);
            }
        }
    }

    fn handle_command(&mut self, hwnd: HWND, code: u32) {
        let Some(id) = MenuId::from_command(code) else {
            return;
        };
        match id {
            MenuId::OpenDashboard => {
                // 审查 P2：冷启动时 dashboard 可能晚于 2s 才落到回退段端口
                //（如 18422），点菜单时以 dashboard-port.txt 为准，避免打开
                // 请求端口的死链接。
                let port = dashboard_port_actual(self.args.port);
                let url: Vec<u16> = format!("http://127.0.0.1:{}\0", port)
                    .encode_utf16()
                    .collect();
                open_with_shell(&url);
            }
            MenuId::TogglePause => match self.state {
                TrayState::Running => {
                    // Pause:置停机旗标 + join 在属主线程侧完成;UI 即时反馈
                    let _ = self.cmd_tx.send(CollectorCmd::Pause);
                    self.set_state(hwnd, TrayState::Paused);
                }
                TrayState::Paused | TrayState::Error => {
                    let _ = self.cmd_tx.send(CollectorCmd::Resume);
                    self.set_state(hwnd, TrayState::Running);
                }
            },
            MenuId::OpenDataFolder => {
                // DB 所在目录;无父目录时退回 cwd
                let dir = self
                    .args
                    .db
                    .parent()
                    .map(|p| p.to_path_buf())
                    .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
                use std::os::windows::ffi::OsStrExt;
                let mut wide: Vec<u16> = dir.as_os_str().encode_wide().collect();
                wide.push(0);
                open_with_shell(&wide);
            }
            MenuId::UpdateNow => {
                // 一键更新：安装版跳下载页（安装版原地自更新会造成卸载数据库
                // 版本漂移，被 update 拒绝）；便携版直接跑 `kynoptic update`
                //（它自己会杀托盘/替换/拉回）。
                let exe_dir = std::env::current_exe()
                    .ok()
                    .and_then(|e| e.parent().map(|p| p.to_path_buf()));
                let installed = exe_dir
                    .as_ref()
                    .map(|d| d.join("unins000.exe").exists())
                    .unwrap_or(false);
                if installed {
                    let url: Vec<u16> =
                        String::from("https://github.com/ardss/kynoptic/releases/latest\0")
                            .encode_utf16()
                            .collect();
                    open_with_shell(&url);
                } else if let Some(dir) = exe_dir {
                    use std::os::windows::process::CommandExt;
                    const DETACHED_PROCESS: u32 = 0x0000_0008;
                    // 输出落档（全库审查 P1：一键更新此前全程静默，失败无人知）
                    if let Some(db_dir) = self.args.db.parent() {
                        use std::io::Write;
                        let open = || {
                            std::fs::OpenOptions::new()
                                .create(true)
                                .append(true)
                                .open(db_dir.join("update.log"))
                                .ok()
                        };
                        if let Some(mut log) = open() {
                            let _ = writeln!(
                                log,
                                "--- update requested {} ---",
                                chrono::Local::now().to_rfc3339()
                            );
                            let errlog = open().unwrap_or_else(|| {
                                std::fs::File::create(db_dir.join("update.log")).unwrap()
                            });
                            let _ = std::process::Command::new(dir.join("kynoptic.exe"))
                                .arg("update")
                                .stdout(log)
                                .stderr(errlog)
                                .creation_flags(DETACHED_PROCESS)
                                .spawn();
                        }
                    }
                }
            }
            MenuId::About => {
                let url: Vec<u16> = String::from("https://github.com/ardss/kynoptic\0")
                    .encode_utf16()
                    .collect();
                open_with_shell(&url);
            }
            MenuId::Quit => {
                // 优雅关停:命令通道通知属主线程置停机旗标并 join writer,
                // 然后删图标、PostQuitMessage 结束消息循环。
                let _ = self.cmd_tx.send(CollectorCmd::Quit);
                unsafe {
                    DestroyWindow(hwnd);
                }
            }
        }
    }
}

/// ShellExecuteW 打开 URL 或目录(只读 shell 动作,绝不注入合成输入)。
fn open_with_shell(path_wide: &[u16]) {
    unsafe {
        let verb: Vec<u16> = "open\0".encode_utf16().collect();
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            path_wide.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        );
    }
}

fn append_item(menu: win::HMENU, id: MenuId, state: TrayState) {
    unsafe {
        let text: Vec<u16> = format!("{}\0", id.label(state)).encode_utf16().collect();
        AppendMenuW(menu, MF_STRING, id as usize, text.as_ptr());
    }
}

fn tray_base(hwnd: HWND) -> NOTIFYICONDATAW {
    let mut nid: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
    nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    nid.hWnd = hwnd;
    nid.uID = TRAY_ID;
    nid
}

fn set_tip(nid: &mut NOTIFYICONDATAW, tip: &str) {
    let mut i = 0usize;
    for u in tip.encode_utf16().take(127) {
        nid.szTip[i] = u;
        i += 1;
    }
    nid.szTip[i] = 0;
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let ctx_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut TrayCtx;
    // SAFETY: 窗口创建时经 GWLP_USERDATA 挂入的唯一 TrayCtx；Timer 分支需要
    // 可变引用同步 Error/Running 图标。
    let mut ctx: Option<&mut TrayCtx> = unsafe { ctx_ptr.as_mut() };
    // Timer 分支需可变引用同步 Error/Running 图标
    let mut ctx = ctx.as_mut();

    match msg {
        WM_TRAYICON => {
            if let Some(ctx) = ctx_ptr.as_mut() {
                let m = lparam as u32;
                if m == WM_RBUTTONUP || m == WM_CONTEXTMENU {
                    ctx.show_menu(hwnd);
                } else if m == WM_LBUTTONDBLCLK || m == 0x0202 {
                    // 单击/双击都直接开面板（交互审查：主操作不要求用户知道双击约定，左键=主行为是托盘惯例）
                    ctx.handle_command(hwnd, MenuId::OpenDashboard as u32);
                }
            }
            0
        }
        0x0113 => {
            // WM_TIMER：同步 dashboard 健康旗标到 Error/Running 图标
            //（dash 线程不能直接碰 UI；托盘三态此前是死代码，定性审查接线）
            let failed = crate::DASH_FAILED.load(std::sync::atomic::Ordering::Relaxed);
            let want = match failed {
                1 => Some(TrayState::Error),
                2 => Some(TrayState::Running),
                _ => None,
            };
            if let Some(w) = want {
                if let Some(c) = ctx.as_mut() {
                    if c.state != w && c.state != TrayState::Paused {
                        c.set_state(hwnd, w);
                    }
                }
            }
            0
        }
        WM_COMMAND => {
            if let Some(ctx) = ctx_ptr.as_mut() {
                ctx.handle_command(hwnd, (wparam & 0xFFFFusize) as u32);
            }
            0
        }
        // 系统关机/注销：默认处理会直接放行强杀，丢掉 writer 内存批里
        // 最多一个 flush 周期的事件（审查 P1）。发 Quit 走与菜单退出同一
        // 条优雅关停链（排空通道+join writer+关 session），系统留给我们
        // 的宽限窗口足够 writer 排空。
        WM_QUERYENDSESSION => {
            let ctx_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA);
            if ctx_ptr != 0 {
                if let Some(ctx) = unsafe { (ctx_ptr as *mut TrayCtx).as_mut() } {
                    let _ = ctx.cmd_tx.send(CollectorCmd::Quit);
                }
            }
            1 // 允许关机会话继续
        }
        WM_ENDSESSION => {
            if wparam != 0 {
                let _ = unsafe { DestroyWindow(hwnd) };
            }
            0
        }
        WM_DESTROY => {
            let nid = tray_base(hwnd);
            Shell_NotifyIconW(NIM_DELETE, &nid);
            PostQuitMessage(0);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// 注册托盘图标并进入消息循环(阻塞直到 Quit)。
/// 返回 false 表示托盘不可用(调用方应退出)。
pub fn run(args: Args, cmd_tx: Sender<CollectorCmd>) -> bool {
    unsafe {
        let hinstance =
            windows_sys::Win32::System::LibraryLoader::GetModuleHandleW(std::ptr::null());
        let class_name: Vec<u16> = "KynopticTray\0".encode_utf16().collect();
        let wc = WNDCLASSW {
            style: 0,
            lpfnWndProc: Some(wndproc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinstance,
            hIcon: std::ptr::null_mut(),
            hCursor: LoadCursorW(std::ptr::null_mut(), win::IDC_ARROW),
            hbrBackground: std::ptr::null_mut(),
            lpszMenuName: std::ptr::null(),
            lpszClassName: class_name.as_ptr(),
        };
        if RegisterClassW(&wc) == 0 {
            eprintln!("kynoptic-tray: RegisterClassW failed");
            return false;
        }

        // 不可见顶层窗口(零尺寸、不显示)
        let title: Vec<u16> = "Kynoptic tray\0".encode_utf16().collect();
        let hwnd = CreateWindowExW(
            0, // WINDOW_EX_STYLE
            class_name.as_ptr(),
            title.as_ptr(),
            WS_OVERLAPPED,
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
            eprintln!("kynoptic-tray: CreateWindowExW failed");
            return false;
        }
        ShowWindow(hwnd, SW_HIDE);
        // 2s 轮询 dash 健康旗标（0x0113 分支同步 Error/Running 图标）
        SetTimer(hwnd, 1, 2000, None);

        let Some(icons) = TrayIcons::create() else {
            eprintln!("kynoptic-tray: icon drawing failed");
            return false;
        };

        // NIM_ADD:注册托盘图标;此后仅 NIM_MODIFY 换态,NIF_INFO 永不使用
        let mut nid = tray_base(hwnd);
        nid.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
        nid.uCallbackMessage = WM_TRAYICON;
        nid.hIcon = icons.for_state(TrayState::Running);
        set_tip(&mut nid, "Kynoptic: collecting");
        if Shell_NotifyIconW(NIM_ADD, &nid) == 0 {
            eprintln!("kynoptic-tray: Shell_NotifyIconW(NIM_ADD) failed");
            return false;
        }

        // 上下文挂在窗口上;进程生命周期即托盘生命周期,Box::leak 有意为之
        let ctx = Box::new(TrayCtx {
            icons,
            cmd_tx,
            state: TrayState::Running,
            args,
        });
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, Box::into_raw(ctx) as isize);

        // 消息循环:程序无主窗口,这里即全部生命周期
        let mut msg: win::MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        true
    }
}

#[cfg(test)]
mod fault_injection_tests {
    //! 第 24 轮故障注入演练：update-available.txt 投毒防御的行为钉子。
    //! 契约（SKILL.md/审查记录）：读不到/内容怪 = 无更新，绝不 panic。
    use super::{update_available_version, version_gt};

    fn poisoned(content: &[u8]) -> Option<String> {
        let dir = std::env::temp_dir().join(format!(
            "kyn-r24-poison-{}-{}",
            std::process::id(),
            std::thread::current()
                .name()
                .unwrap_or("t")
                .replace("::", "-")
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("kynoptic.db");
        std::fs::write(dir.join("update-available.txt"), content).unwrap();
        let r = update_available_version(&db);
        let _ = std::fs::remove_dir_all(&dir);
        r
    }

    #[test]
    fn binary_garbage_is_no_update() {
        assert_eq!(poisoned(&[0xFF, 0xFE, 0x00, 0x93]), None);
    }

    #[test]
    fn empty_and_textual_junk_is_no_update() {
        assert_eq!(poisoned(b""), None);
        assert_eq!(poisoned(b"abc"), None);
        assert_eq!(poisoned(b"x1234"), None); // 首字符非数字
    }

    #[test]
    fn plausible_future_version_is_banner() {
        // 唯一能挂出提示的形态：三段数字且 > 当前版本（0.2.0）
        assert_eq!(poisoned(b"99.0.0\n").as_deref(), Some("99.0.0"));
        assert_eq!(poisoned(b"v99.0.0").as_deref(), Some("99.0.0"));
    }

    #[test]
    fn older_version_is_no_update() {
        assert_eq!(poisoned(b"0.1.0"), None);
        assert_eq!(poisoned(b"0.2.0"), None); // 等于当前版本不算新
    }

    #[test]
    fn oversized_segment_overflow_parses_to_zero_not_banner() {
        // >u64 的段 parse 失败按 0 处理 → 不满足 > 当前版本 → 不挂提示
        assert_eq!(poisoned(b"99999999999999999999999999.0.0"), None);
    }

    #[test]
    fn ten_megabyte_digit_wall_is_handled() {
        let big = vec![b'9'; 10 * 1024 * 1024];
        // 无点号 → 单段 parse 溢出为 0 → None；即便未来改成多段也不应 panic
        assert_eq!(poisoned(&big), None);
    }

    #[test]
    fn version_gt_semantics() {
        assert!(version_gt("0.3.0", "0.2.0"));
        assert!(version_gt("99.0.0", "0.2.0"));
        assert!(!version_gt("0.2.0", "0.2.0"));
        assert!(!version_gt("0.1.9", "0.2.0"));
        // 短段按 0 补齐
        assert!(version_gt("1.0", "0.99.9"));
        // 预发布后缀忽略（-rc1），解析失败段按 0
        assert!(version_gt("0.3.0-rc1", "0.2.0"));
    }

    #[test]
    fn zero_version_and_malformed_prefixes_are_no_update() {
        // r25 混沌演练补充：三态机边界
        // "0.0.0" 是合法三段数字但 <= 当前版本 → 无更新
        assert_eq!(poisoned(b"0.0.0"), None);
        // 当前版本自身（带 v 前缀）→ 相等不算新
        assert_eq!(poisoned(b"v0.2.0"), None);
        // v 与版本号之间混入空格：trim 后 trim_start_matches('v') 只剥 'v'
        // 本身，剩下的 " 0.2.0" 首字符非数字 → 按内容怪处理（无更新）
        assert_eq!(poisoned(b"v 0.2.0"), None);
        // 前导空白会被 trim() 吃掉：仍能解析并正常比较
        assert_eq!(poisoned(b"  v99.0.0\n").as_deref(), Some("99.0.0"));
        // v 前缀重复：trim_start_matches 贪婪剥离，"vv0.2.0" → "0.2.0" = 当前
        assert_eq!(poisoned(b"vv0.2.0"), None);
        // 制表符/换行混排 = 内容怪
        assert_eq!(poisoned(b"\t99.0.0\r\n").as_deref(), Some("99.0.0"));
        assert_eq!(poisoned(b"9\t9.0.0"), None);
    }

    #[test]
    fn missing_file_and_missing_dir_are_no_update() {
        let dir = std::env::temp_dir().join(format!("kyn-r24-poison-none-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("kynoptic.db");
        assert_eq!(update_available_version(&db), None);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(update_available_version(&db), None); // 目录也没了
    }
}
