//! PowerShell 命令执行辅助（PS 子进程实现，powershell spawn，待原生 API 重写）

/// 执行 PowerShell 脚本并返回标准输出。
///
/// 语义（2026-09-09 probe 实测修正）：
/// 1. **不依赖退出码**——`powershell -Command` 在 cmdlet 产生"被 SilentlyContinue
///    压制的非终止错误"时（典型：Get-WinEvent 无匹配记录），即便 stdout 有有效
///    数据也以退出码 1 结束。原实现因此把 driver 等事件日志型监控器的有效输出
///    整批丢弃（表现：探针零事件、无任何日志）。现改为：stdout 非空即返回数据。
/// 2. **强制 UTF-8 输出**——powershell 管道输出默认走控制台 OEM 代码页（中文系统
///    为 GBK），`from_utf8_lossy` 会把中文设备名/消息打成乱码。统一在脚本前注入
///    `[Console]::OutputEncoding=UTF8`，stdout 才是 UTF-8 字节。
pub fn run_ps(script: &str) -> Option<String> {
    // 输出编码必须是脚本的第一条语句（对后续管道生效）
    let prefixed = format!(
        "[Console]::OutputEncoding=[System.Text.Encoding]::UTF8; {}",
        script
    );
    let output = std::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &prefixed])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    if stdout.trim().is_empty() {
        None
    } else {
        Some(stdout)
    }
}
