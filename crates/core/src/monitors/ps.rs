//! PowerShell 命令执行辅助（PS 子进程实现，powershell spawn，待原生 API 重写）

use std::io::Read;
use std::time::{Duration, Instant};

/// 单次 PS 采集的截止时限：WMI/Winmgmt 卡死时不能让采集线程连同
/// 子进程与管道句柄无限期滞留（超时后 kill 并按无数据处理）。
const PS_TIMEOUT: Duration = Duration::from_secs(60);

/// run_ps 三态结果（monitor 级降级信号，审查：PS 依赖监控器在
/// powershell.exe 被杀软/WDAC 拦截后永远静默无数据，设置页仍显示已启用，
/// 用户无从区分"没这传感器"与"被拦截"）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PsOutcome {
    /// 拿到 stdout 数据
    Data(String),
    /// 子进程正常跑完但没有有效输出（脚本无匹配记录等正常情况）
    NoData,
    /// 子进程无法启动/等待出错——依赖性故障，计入 MONITOR_DEGRADED
    LaunchFailed(String),
}

/// 执行 PowerShell 脚本并返回三态结果。调用方可据此区分"无数据"与
/// "启动失败"并触发降级告警。
pub fn run_ps_detailed(script: &str) -> PsOutcome {
    match run_ps_inner(script) {
        Ok(Some(s)) => PsOutcome::Data(s),
        Ok(None) => PsOutcome::NoData,
        Err(e) => {
            super::note_degraded(&format!("powershell 启动失败: {e}"));
            PsOutcome::LaunchFailed(e)
        }
    }
}

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
/// 3. **带超时**——`.output()` 会同步等子进程退出，Winmgmt 卡死时采集线程
///    将被无限期滞留；改为 spawn + try_wait 轮询，超时 kill 后返回 None。
///
/// 兼容包装：老调用方只需要"有数据/无数据"两态，启动失败同样折叠为 None，
/// 但降级会计入 [`super::MONITOR_DEGRADED`]（见 [`run_ps_detailed`]）。
pub fn run_ps(script: &str) -> Option<String> {
    run_ps_inner(script).ok().flatten()
}

/// run_ps 的实现体：Err = spawn/wait 层面的失败（依赖被拦截等）。
fn run_ps_inner(script: &str) -> Result<Option<String>, String> {
    // 输出编码必须是脚本的第一条语句（对后续管道生效）
    let prefixed = format!(
        "[Console]::OutputEncoding=[System.Text.Encoding]::UTF8; {}",
        script
    );
    let mut child = match super::quiet_command("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &prefixed])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        // 降级留痕：spawn 失败（powershell 缺失/被杀软拦截）此前被吞成
        // None，PS 依赖监控器开启后永远静默无数据
        Err(e) => return Err(e.to_string()),
    };
    let deadline = Instant::now() + PS_TIMEOUT;
    // 轮询等待退出；超时则 kill，避免线程/句柄滞留
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if Instant::now() >= deadline {
                    // 约束:超时必须先 kill 再 wait,回收子进程与管道句柄
                    let _ = child.kill();
                    let _ = child.wait();
                    return Ok(None);
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(_) => return Err("powershell try_wait 失败".into()),
        }
    }
    // 进程已退出,管道不会再写入,此刻读尽 stdout 不会阻塞
    let mut stdout_bytes = Vec::new();
    if let Some(mut pipe) = child.stdout.take() {
        if pipe.read_to_end(&mut stdout_bytes).is_err() {
            return Ok(None);
        }
    }
    let stdout = String::from_utf8_lossy(&stdout_bytes).to_string();
    if stdout.trim().is_empty() {
        Ok(None)
    } else {
        Ok(Some(stdout))
    }
}
