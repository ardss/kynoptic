//! v0.1 监控器（14 个，全部纯 windows-sys、零 PowerShell 子进程）
//!
//! 轮询型（12）：system / window / idle / session / battery / network /
//! device / process / audio / brightness / wifi / power_plan
//! 事件驱动 Hook（2）：keyboard_hook / mouse_hook

// 轮询型监控器
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

// 事件驱动型 Hook
pub mod keyboard_hook;
pub mod mouse_hook;
