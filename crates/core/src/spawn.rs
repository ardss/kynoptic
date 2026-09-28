//! 子进程创建旗标的单一权威：「不弹控制台窗口」。
//!
//! 仓库铁律：任何会开子进程的控制台类工具调用（powershell / cmd / tasklist /
//! taskkill / schtasks / python / kynoptic.exe 等）一律经 [`no_window`] 设旗标，
//! 禁止在调用点散落写 `creation_flags(CREATE_NO_WINDOW)` 或 `DETACHED_PROCESS`——
//! 散落写法正是历史上闪黑框事故的根因（W42 起多轮修复均因漏点复发）：
//! 托盘常驻进程、计划任务里跑出的任何控制台子进程，缺了该旗标就闪框。
//!
//! 旗标写入纪律：[`no_window`] 是创建旗标的**唯一写入方**，调用点不得在调用
//! 前自行 `.creation_flags(...)`（std 的 Command 不暴露读取，也无法在其上 OR
//! 合）。GUI 子系统进程（kynoptic-tray.exe 等）过一遍同样无害，调用点无需
//! 判断目标是控制台还是 GUI，统一走这里即可。

/// 为 `cmd` 置「不新建控制台窗口」创建旗标。
/// 非 Windows 平台为无操作；重复调用无副作用。
pub fn no_window(cmd: &mut std::process::Command) -> &mut std::process::Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 重复调用不产生副作用（非 Windows 上为纯无操作）。
    #[test]
    fn idempotent_on_non_windows() {
        let mut c = std::process::Command::new("true");
        no_window(&mut c);
        no_window(&mut c);
    }

    /// 行为测试：置旗标后子进程照常可启动并正常退出。
    #[cfg(windows)]
    #[test]
    fn still_spawns_quiet_child() {
        let mut c = std::process::Command::new("cmd");
        c.args(["/c", "exit", "0"]);
        no_window(&mut c);
        no_window(&mut c);
        let out = c.output().expect("no_window 后的子进程必须可启动");
        assert!(out.status.success(), "quiet child 必须正常退出");
    }
}
