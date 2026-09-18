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
    RegisterClassW, SetForegroundWindow, SetWindowLongPtrW, ShowWindow, TrackPopupMenu,
    TranslateMessage, GWLP_USERDATA, TPM_BOTTOMALIGN, TPM_LEFTALIGN, TPM_RETURNCMD, WM_APP,
    WM_COMMAND, WM_DESTROY, WM_RBUTTONUP, WNDCLASSW, WS_OVERLAPPED,
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
        Some(v.to_string())
    } else {
        None
    }
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
            TrayState::Running => "Kynoptic: collecting · 右键菜单",
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
                let text: Vec<u16> = format!("Update available: v{} - click to install\0", ver)
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
                    let _ = std::process::Command::new(dir.join("kynoptic.exe"))
                        .arg("update")
                        .creation_flags(DETACHED_PROCESS)
                        .spawn();
                }
            }
            MenuId::About => {
                let url: Vec<u16> = String::from("https://github.com/ardss/kynoptic ")
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
    match msg {
        WM_TRAYICON => {
            if let Some(ctx) = ctx_ptr.as_mut() {
                let m = lparam as u32;
                if m == WM_RBUTTONUP || m == WM_CONTEXTMENU {
                    ctx.show_menu(hwnd);
                } else if m == WM_LBUTTONDBLCLK {
                    // WM_LBUTTONDBLCLK:双击直接开面板
                    ctx.handle_command(hwnd, MenuId::OpenDashboard as u32);
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
