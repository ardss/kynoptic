//! 监控器全集（40 个）：14 个默认启用（纯 windows-sys、零子进程）+
//! 26 个已恢复但默认关闭（R8 ②/③ 级，含隐私开关或 PS 子进程，可配置启用）。
//!
//! 默认开关的单一事实源是 [`crate::registry`]（`MONITOR_REGISTRY`），
//! `crates/core/config/monitors.json` 模板由测试保证与之一致。
//!
//! ## 默认启用（14，全部原生、零 PowerShell）
//! 轮询型（12）：system / window / idle / session / battery / network /
//! device / process / audio / brightness / wifi / power_plan
//! 事件驱动 Hook（2）：keyboard_hook / mouse_hook
//!
//! ## 已恢复、默认关闭（26）
//! - 纯 windows-sys（W）：browser / clipboard / file_activity / media /
//!   screen_capture / usb_device / bluetooth
//! - PS 子进程（默认关、待原生重写）：thermal / gpu / display /
//!   external_display / audio_input / audio_output / ime / vpn / dns /
//!   notification / calendar / location / print / firewall / security /
//!   uac / windows_update / driver / stylus

// ── 默认启用：轮询型监控器（原生）──
pub mod audio;
pub mod battery;
pub mod brightness;
pub mod device;
pub mod idle;
pub mod network;
pub mod power_plan;
pub mod process;
pub mod session;
pub mod system;
pub mod wifi;
pub mod window;

// ── 默认启用：事件驱动 Hook（原生）──
pub mod keyboard_hook;
pub mod mouse_hook;

// ── 已恢复、默认关闭：纯 windows-sys（原生，无子进程）──
pub mod bluetooth;
pub mod browser;
pub mod clipboard;
pub mod file_activity;
pub mod media;
pub mod screen_capture;
pub mod usb_device;

// ── 已恢复、默认关闭：PS 子进程依赖（待原生重写）──
pub mod audio_input;
pub mod audio_output;
pub mod calendar;
pub mod display;
pub mod dns;
pub mod driver;
pub mod external_display;
pub mod firewall;
pub mod gpu;
pub mod ime;
pub mod location;
pub mod notification;
pub mod print;
pub mod ps;
pub mod security;
pub mod stylus;
pub mod thermal;
pub mod uac;
pub mod vpn;
pub mod windows_update;

/// 构造不带控制台窗口的子进程命令：托盘/后台常驻进程轮询外部工具
/// （nvidia-smi、powercfg、netstat、netsh、powershell）时，
/// 不加 `CREATE_NO_WINDOW` 会每次采集都闪一个黑框。
pub fn quiet_command(program: &str) -> std::process::Command {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let mut cmd = std::process::Command::new(program);
    cmd.creation_flags(CREATE_NO_WINDOW);
    cmd
}
