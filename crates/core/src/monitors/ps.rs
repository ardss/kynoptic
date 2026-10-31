//! PowerShell 命令执行辅助（持久宿主 + 单次 spawn 回退，待原生 API 重写）
//!
//! 进程上界约束：稳态下至多 1 个常驻宿主进程 + 至多 [`PS_SPAWN_LANE_MAX`]
//! 个瞬时单次子进程。宿主通过全局互斥锁串行化宿主路径调用（单进程的
//! stdin/stdout 不允许并发写入交错），锁同时保护宿主状态。
//!
//! 双车道（40 项全开排队饥饿根治，KY-PERF-20261031）：宿主锁空闲时走宿主
//! 车道（零额外进程）；锁被占用（上一条脚本执行中/挂死）时立即改走有界
//! 并发 spawn 道独立执行，不再在引擎锁上排队——排队等待曾是首拍 10s 截止
//! 失守与 60s 超时连锁的直接根因（17 个 PS 采集器共享单通道完全串行）。

use std::io::{Read, Write};
use std::process::{Child, ChildStdin, ChildStdout};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::RecvTimeoutError;
use std::sync::{mpsc, Mutex};
use std::time::{Duration, Instant};

/// 单次 PS 采集的截止时限：WMI/Winmgmt 卡死或宿主挂死时不能让采集线程
/// 连同子进程与管道句柄无限期滞留（超时后 kill 并按无数据处理）。
const PS_TIMEOUT: Duration = Duration::from_secs(60);

/// spawn 道并发上界（瞬时单次子进程数）。宿主忙时最多再有 3 个脚本并发
/// 直跑；许可耗尽时回退为在宿主锁上排队（绝不拒绝调用，最坏退化为旧行为）。
const PS_SPAWN_LANE_MAX: usize = 3;

/// 计数信号量（spawn 道许可）。RAII 许可：guard drop 即释放。
struct LaneSemaphore {
    remaining: Mutex<usize>,
}
impl LaneSemaphore {
    const fn new(n: usize) -> Self {
        LaneSemaphore {
            remaining: Mutex::new(n),
        }
    }
    /// 取许可；耗尽返回 None（调用方回退宿主锁排队）。
    fn clone_try_acquire(self: &std::sync::Arc<Self>) -> Option<LanePermit> {
        let mut g = self.remaining.lock().unwrap_or_else(|p| p.into_inner());
        if *g == 0 {
            return None;
        }
        *g -= 1;
        Some(LanePermit { sem: self.clone() })
    }
}
/// RAII 许可。Arc 共享便于测试持有多个许可。
#[derive(Clone)]
struct LanePermit {
    sem: std::sync::Arc<LaneSemaphore>,
}
impl Drop for LanePermit {
    fn drop(&mut self) {
        let mut g = self.sem.remaining.lock().unwrap_or_else(|p| p.into_inner());
        *g += 1;
    }
}

static SPAWN_LANE: std::sync::OnceLock<std::sync::Arc<LaneSemaphore>> = std::sync::OnceLock::new();
fn spawn_lane() -> &'static std::sync::Arc<LaneSemaphore> {
    SPAWN_LANE.get_or_init(|| std::sync::Arc::new(LaneSemaphore::new(PS_SPAWN_LANE_MAX)))
}

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
/// 语义约束：
/// 1. **不依赖退出码**——`powershell -Command` 在 cmdlet 产生"被 SilentlyContinue
///    压制的非终止错误"时（典型：Get-WinEvent 无匹配记录），即便 stdout 有有效
///    数据也以退出码 1 结束。因此：stdout 非空即返回数据。
/// 2. **强制 UTF-8 输出**——powershell 管道输出默认走控制台 OEM 代码页（中文系统
///    为 GBK），`from_utf8_lossy` 会把中文设备名/消息打成乱码。统一在脚本前注入
///    `[Console]::OutputEncoding=UTF8`，stdout 才是 UTF-8 字节。
/// 3. **带超时**——每次调用带截止时间，超时 kill 相关进程后按无数据处理，
///    采集线程绝不无限期滞留。
/// 4. **并发读**——stdout 必须由独立线程与子进程存活期并发读：脚本输出超过
///    管道缓冲（约 64KB）时，"先等退出再读"会让写入方与等待方互卡到超时丢数据。
///
/// 兼容包装：老调用方只需要"有数据/无数据"两态，启动失败同样折叠为 None，
/// 但降级会计入 [`super::MONITOR_DEGRADED`]（见 [`run_ps_detailed`]）。
pub fn run_ps(script: &str) -> Option<String> {
    run_ps_inner(script).ok().flatten()
}

/// run_ps 的实现体：Err = spawn 层面的失败（依赖被拦截等）。
/// 编码前缀在本层注入，宿主/回退两条路径共用同一 UTF-8 语义。
fn run_ps_inner(script: &str) -> Result<Option<String>, String> {
    // 输出编码必须是脚本的第一条语句（对后续管道生效）
    let prefixed = format!("[Console]::OutputEncoding=[System.Text.Encoding]::UTF8; {script}");
    // 回退模式只借锁读标记，单次 spawn 经有界 spawn 道跑（并发上界）
    if let Ok(mut guard) = ENGINE.try_lock() {
        if guard.permanent_fallback {
            // 测试注入点：回退可执行名借锁读出后锁外执行
            let prog = guard.fallback_program().to_string();
            drop(guard);
            return run_in_spawn_lane(&prog, &prefixed);
        }
        return guard.execute(&prefixed, PS_TIMEOUT);
    }
    // 宿主锁被占（上一条脚本执行中/挂死）：不再排队，改走有界 spawn 道独立
    // 执行——单通道串行曾让队尾等待本身超首拍截止并触发 60s 连锁超时
    run_in_spawn_lane("powershell", &prefixed)
}

/// 有界 spawn 道执行：拿到许可直跑；许可耗尽回退宿主锁排队（绝不拒绝）。
fn run_in_spawn_lane(program: &str, prefixed: &str) -> Result<Option<String>, String> {
    let permit = spawn_lane().clone_try_acquire();
    match permit {
        Some(_p) => spawn_ps_once(program, prefixed, PS_TIMEOUT),
        None => {
            let mut guard = ENGINE.lock().unwrap_or_else(|p| p.into_inner());
            guard.execute(prefixed, PS_TIMEOUT)
        }
    }
}

/// 宿主单次执行失败的分类。
enum HostFailure {
    /// 宿主进程退出/管道断裂（写入或读取失败）
    Dead,
    /// 截止时间内未见哨兵（宿主挂死或脚本卡死）
    TimedOut,
}

/// 常驻 PowerShell 宿主：单进程复用，脚本经 stdin 逐条投递，
/// 每条输出以唯一哨兵行定界。
struct PwshHost {
    child: Child,
    stdin: ChildStdin,
    /// stdout 句柄在两次调用间归还暂存（读线程每次取走、结束后经通道归还）
    stdout: Option<ChildStdout>,
    /// 哨兵单调计数（叠加上层 pid 保证唯一）
    seq: AtomicU64,
}

impl PwshHost {
    /// 启动宿主进程。约束:创建旗标必须经 [`crate::spawn::no_window`]
    /// 收口（仓库铁律），此处经 quiet_command 间接满足。
    fn spawn(program: &str) -> std::io::Result<PwshHost> {
        let mut child = super::quiet_command(program)
            .args(["-NoProfile", "-NonInteractive", "-Command", "-"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()?;
        let stdin = child.stdin.take().expect("stdin 已置 piped");
        let stdout = child.stdout.take().expect("stdout 已置 piped");
        Ok(PwshHost {
            child,
            stdin,
            stdout: Some(stdout),
            seq: AtomicU64::new(0),
        })
    }

    /// 宿主是否已退出（外部被杀/自行退出）。try_wait 顺带完成状态回收。
    fn is_dead(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(Some(_)))
    }

    /// 强制终止并收尸，回收子进程与管道句柄。
    fn kill_reap(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    /// 执行一条脚本，返回哨兵行之前的全部 stdout。
    /// 超时/失联时 kill 收尸后返回对应失败分类（宿主随后被引擎整体丢弃）。
    fn exec(&mut self, script: &str, timeout: Duration) -> Result<String, HostFailure> {
        // 哨兵每次调用唯一：上层 pid + 单调计数，正常脚本输出不可能命中
        let sentinel = format!(
            "__kynoptic_ps_end_{}_{:x}__",
            std::process::id(),
            self.seq.fetch_add(1, Ordering::Relaxed),
        );
        // 脚本经 base64+Invoke-Expression 投递：stdin 上只有纯 ASCII，不依赖
        // PowerShell 对管道输入的代码页解码（vpn 等脚本含中文匹配文本，走
        // 原文投递会在 GBK 代码页下被打成乱码改变语义）；try/catch 保证
        // 脚本终止性出错后哨兵必然输出。约束:脚本不得自行读 stdin 或 exit
        // （前者会吃掉后续投递，后者会杀宿主——两者都会触发降级重生）。
        let b64 = base64_encode(script.as_bytes());
        let submission = format!(
            "$__kynoptic_ps=[Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('{b64}'));\
             try{{Invoke-Expression $__kynoptic_ps}}catch{{}};\
             Write-Output '';Write-Output '{sentinel}'\n"
        );
        let deadline = Instant::now() + timeout;
        // 读线程先于写入启动：输出超过管道缓冲时写入方也不会被读侧卡死
        let (tx, rx) = mpsc::channel();
        let mut pipe = self.stdout.take().expect("stdout 句柄必须可取用");
        let reader = {
            let sentinel = sentinel.clone();
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 8192];
                let mut found = false;
                while !found {
                    match pipe.read(&mut chunk) {
                        Ok(0) => break,
                        Ok(n) => {
                            buf.extend_from_slice(&chunk[..n]);
                            found = find_sentinel(&buf, sentinel.as_bytes()).is_some();
                        }
                        Err(_) => break,
                    }
                }
                let _ = tx.send((buf, found, pipe));
            })
        };
        // 脚本体上限约 1.5KB（生产脚本实测最长 732B），远小于 stdin 管道缓冲，
        // 宿主此刻必然在等输入，写入不会阻塞
        let write_res = self
            .stdin
            .write_all(submission.as_bytes())
            .and_then(|()| self.stdin.flush());
        if write_res.is_err() {
            // 管道断裂：宿主已死
            self.kill_reap();
            let _ = reader.join();
            return Err(HostFailure::Dead);
        }
        let remain = deadline.saturating_duration_since(Instant::now());
        if remain.is_zero() {
            self.kill_reap();
            let _ = reader.join();
            return Err(HostFailure::TimedOut);
        }
        match rx.recv_timeout(remain) {
            Ok((buf, found, pipe)) => {
                self.stdout = Some(pipe);
                let _ = reader.join();
                if found {
                    return Ok(cut_before_sentinel(&buf, sentinel.as_bytes()));
                }
                // 管道已关但未见哨兵：宿主中途退出
                self.kill_reap();
                Err(HostFailure::Dead)
            }
            Err(RecvTimeoutError::Timeout) => {
                self.kill_reap();
                let _ = reader.join();
                Err(HostFailure::TimedOut)
            }
            Err(RecvTimeoutError::Disconnected) => {
                // 读线程异常终止（panic），宿主状态不可信
                self.kill_reap();
                Err(HostFailure::Dead)
            }
        }
    }
}

/// 在缓冲中定位哨兵行首。约束:哨兵必须前置换行符——投递协议靠
/// `Write-Output ''` 保证这一点，行首校验避免输出尾行拼上哨兵时漏判。
fn find_sentinel(buf: &[u8], sentinel: &[u8]) -> Option<usize> {
    if buf.len() < sentinel.len() + 1 {
        return None;
    }
    let end = buf.len() - sentinel.len() - 1;
    (0..=end)
        .find(|&i| buf[i] == b'\n' && &buf[i + 1..i + 1 + sentinel.len()] == sentinel)
        .map(|i| i + 1)
}

/// 截取哨兵行之前的输出，并去掉投递协议注入的空行分隔符
/// （脚本无尾随换行时协议靠 `Write-Output ''` 保证哨兵独立成行）。
fn cut_before_sentinel(buf: &[u8], sentinel: &[u8]) -> String {
    let mut end = find_sentinel(buf, sentinel).unwrap_or(buf.len());
    if end >= 2 && &buf[end - 2..end] == b"\r\n" {
        end -= 2;
    } else if end >= 1 && buf[end - 1] == b'\n' {
        end -= 1;
    }
    String::from_utf8_lossy(&buf[..end]).to_string()
}

/// 标准字母表 base64 编码（带补齐）。只用于脚本投递，不引入外部依赖。
fn base64_encode(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// 单次 spawn 路径（宿主启动失败后的永久回退）。
/// 约束:stdout 由独立线程与子进程存活期并发读（大输出死锁回归防线，
/// 见 [`run_ps`] 文档第 4 条）。
fn spawn_ps_once(program: &str, script: &str, timeout: Duration) -> Result<Option<String>, String> {
    let mut child = match super::quiet_command(program)
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        // 降级留痕：spawn 失败（powershell 缺失/被杀软拦截）此前被吞成
        // None，PS 依赖监控器开启后永远静默无数据
        Err(e) => return Err(e.to_string()),
    };
    let deadline = Instant::now() + timeout;
    // 管道 EOF 即"输出完毕"信号；截止时间内收不到则 kill（kill 会使
    // 管道关闭、读线程随即退出，不会滞留）
    let (tx, rx) = mpsc::channel();
    let reader = {
        let mut pipe = child.stdout.take().expect("stdout 已置 piped");
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let ok = pipe.read_to_end(&mut buf).is_ok();
            let _ = tx.send((buf, ok));
        })
    };
    let remain = deadline.saturating_duration_since(Instant::now());
    if remain.is_zero() {
        let _ = child.kill();
        let _ = child.wait();
        return Ok(None);
    }
    match rx.recv_timeout(remain) {
        Ok((buf, true)) => {
            let _ = reader.join();
            let stdout = String::from_utf8_lossy(&buf).to_string();
            if stdout.trim().is_empty() {
                Ok(None)
            } else {
                Ok(Some(stdout))
            }
        }
        // read 失败：按无数据处理（与原实现一致）
        Ok((_, false)) => Ok(None),
        Err(RecvTimeoutError::Timeout) => {
            let _ = child.kill();
            let _ = child.wait();
            Ok(None)
        }
        Err(RecvTimeoutError::Disconnected) => Ok(None),
    }
}

/// 引擎状态：全局单例。互斥锁承担两件事——宿主协议串行化（单进程的
/// stdin/stdout 不允许并发写入交错）与宿主状态保护；因此宿主模式的
/// 调用全程持锁，上一次采集的截止时间内后续监控线程会在锁上排队。
struct PsEngine {
    host: Option<PwshHost>,
    /// 宿主启动失败后的永久回退标记：置位后每次采集都走单次 spawn，
    /// 不再尝试建宿主（启动失败日志也只留这一条）
    permanent_fallback: bool,
    /// 宿主可执行名（测试注入点；空 = 生产默认 powershell）
    host_program: String,
    /// 回退路径可执行名（测试注入点；空 = 生产默认 powershell）
    fallback_program: String,
}

impl PsEngine {
    const fn production() -> PsEngine {
        PsEngine {
            host: None,
            permanent_fallback: false,
            host_program: String::new(),
            fallback_program: String::new(),
        }
    }

    fn host_program(&self) -> &str {
        if self.host_program.is_empty() {
            "powershell"
        } else {
            &self.host_program
        }
    }

    fn fallback_program(&self) -> &str {
        if self.fallback_program.is_empty() {
            "powershell"
        } else {
            &self.fallback_program
        }
    }

    /// 执行一条脚本（宿主模式）。约束:调用方必须已持有引擎锁。
    fn execute(&mut self, script: &str, timeout: Duration) -> Result<Option<String>, String> {
        // 惰性保活：宿主缺失或已死（外部被杀/自行退出）先重建，本次调用
        // 即可拿到数据——宿主被杀后自动重生是显式契约
        let dead = self.host.as_mut().is_some_and(|h| h.is_dead());
        if dead || self.host.is_none() {
            if let Some(mut old) = self.host.take() {
                old.kill_reap();
                log::warn!("PS 宿主进程已退出，重建宿主");
            }
            match PwshHost::spawn(self.host_program()) {
                Ok(h) => self.host = Some(h),
                Err(e) => {
                    // 宿主建不起来（缺失/被拦截）属于依赖性故障：永久回退
                    // 单次 spawn，只在此留一次日志
                    self.permanent_fallback = true;
                    log::warn!("PS 宿主启动失败({e})，永久回退为单次 spawn 模式");
                    return spawn_ps_once(self.fallback_program(), script, timeout);
                }
            }
        }
        let host = self.host.as_mut().expect("上方已确保宿主存在");
        match host.exec(script, timeout) {
            Ok(out) => {
                if out.trim().is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(out))
                }
            }
            // 挂死/失联：exec 内已完成 kill 收尸，丢弃宿主（下次调用惰性重生），
            // 本次按无数据处理并计入降级
            Err(kind) => {
                self.host = None;
                match kind {
                    HostFailure::TimedOut => super::note_degraded(
                        "PS 宿主无响应（已超时终止），本次按无数据处理，下次调用自动重建",
                    ),
                    HostFailure::Dead => {
                        super::note_degraded("PS 宿主进程失联，本次按无数据处理，下次调用自动重建")
                    }
                }
                Ok(None)
            }
        }
    }
}

static ENGINE: Mutex<PsEngine> = Mutex::new(PsEngine::production());

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试注入点：宿主必然启动失败、回退走真实 powershell 的引擎。
    fn no_host_engine() -> PsEngine {
        PsEngine {
            host_program: "kynoptic-test-missing-powershell".into(),
            ..PsEngine::production()
        }
    }

    /// 测试注入点：宿主与回退都必然启动失败（整个引擎不可用）。
    fn fully_broken_engine() -> PsEngine {
        PsEngine {
            host_program: "kynoptic-test-missing-powershell".into(),
            fallback_program: "kynoptic-test-missing-powershell".into(),
            ..PsEngine::production()
        }
    }

    /// 输出约 200KB 的脚本：远超管道缓冲（约 64KB），大输出死锁回归防线。
    const BIG_SCRIPT: &str = "-join (1..25000 | ForEach-Object { 'abcdefgh' })";

    fn assert_big_data(out: Result<Option<String>, String>) {
        let s = out
            .expect("大输出调用不允许 Err")
            .expect("大输出必须返回数据");
        let trimmed = s.trim();
        assert_eq!(trimmed.len(), 200_000, "输出长度必须完整");
        assert_eq!(
            trimmed.matches("abcdefgh").count(),
            25_000,
            "输出内容必须完整"
        );
        assert!(!trimmed.contains('\u{FFFD}'), "不允许乱码替换符");
    }

    /// 生产同款前缀：强制 UTF-8 输出编码（与 run_ps_inner 注入一致）。
    fn utf8_prefixed(script: &str) -> String {
        format!("[Console]::OutputEncoding=[System.Text.Encoding]::UTF8; {script}")
    }

    /// RED：宿主锁被他人持有时，run_ps 必须经有界并发 spawn 道返回数据，
    /// 而不是在引擎锁上排队到对方放锁（旧实现串行排队，本测试会超时失败）。
    #[cfg(windows)]
    #[test]
    fn busy_host_calls_take_spawn_lane_and_return_data() {
        // guard 留主线程持锁 8s（MutexGuard 非 Send 不跨线程）；run_ps 在
        // worker 线程执行，必须在锁释放前（<5s）经 spawn 道拿到数据
        let g = ENGINE.lock().unwrap();
        let worker = std::thread::spawn(|| {
            let start = std::time::Instant::now();
            let out = run_ps_detailed("Write-Output 'lane-data'");
            (out, start.elapsed())
        });
        std::thread::sleep(std::time::Duration::from_secs(8));
        drop(g);
        let (out, elapsed) = worker.join().unwrap();
        match out {
            PsOutcome::Data(s) => assert!(s.contains("lane-data")),
            other => panic!("spawn 道必须返回数据，实际 {other:?}"),
        }
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "宿主被占时必须走 spawn 道立即执行（实测 {elapsed:?}，旧实现会排队 8s+）"
        );
    }

    /// RED：spawn 道信号量有界——3 许可全占用时 acquire 必须阻塞，
    /// release 后必须唤醒（有界性的单元验证）。
    #[test]
    fn lane_semaphore_is_bounded_and_wakes_on_release() {
        let sem = std::sync::Arc::new(LaneSemaphore::new(2));
        let a = sem.clone_try_acquire();
        let b = sem.clone_try_acquire();
        assert!(a.is_some() && b.is_some(), "两个许可必须立即可取");
        assert!(sem.clone_try_acquire().is_none(), "许可耗尽后不可再取");
        drop(a);
        assert!(
            sem.clone_try_acquire().is_some(),
            "release 后许可必须可再取"
        );
    }

    /// 公共 API 全链路：200KB 大输出必须返回 Data 而非超时（>64KB 管道缓冲，
    /// 读与子进程存活期必须并发）。
    #[cfg(windows)]
    #[test]
    fn large_output_returns_data_via_public_api() {
        match run_ps_detailed(BIG_SCRIPT) {
            PsOutcome::Data(s) => {
                assert_eq!(s.trim().len(), 200_000);
                assert_eq!(s.trim().matches("abcdefgh").count(), 25_000);
            }
            other => panic!("大输出必须返回 Data，实际 {other:?}"),
        }
    }

    /// 宿主路径大输出：独立引擎验证，不与全局单例互相干扰。
    #[cfg(windows)]
    #[test]
    fn large_output_via_host_engine() {
        let mut engine = PsEngine::production();
        assert_big_data(engine.execute(&utf8_prefixed(BIG_SCRIPT), Duration::from_secs(30)));
    }

    /// 回退路径（单次 spawn）大输出：读与子进程存活期并发，超时前必须拿全数据。
    /// 若并发读回归，本调用会卡到截止时间后按无数据处理而失败。
    #[cfg(windows)]
    #[test]
    fn large_output_via_spawn_per_call() {
        let mut engine = no_host_engine();
        assert_big_data(engine.execute(&utf8_prefixed(BIG_SCRIPT), Duration::from_secs(25)));
    }

    /// 宿主被外部 kill 后，下一次调用必须自动重建宿主并正常出数。
    #[cfg(windows)]
    #[test]
    fn host_killed_then_auto_respawns() {
        let mut engine = PsEngine::production();
        let out = engine
            .execute(
                &utf8_prefixed("Write-Output 'kynoptic-alive-1'"),
                Duration::from_secs(30),
            )
            .expect("首次调用不允许 Err")
            .expect("首次调用必须拿到数据");
        assert_eq!(out.trim(), "kynoptic-alive-1");
        {
            let host = engine.host.as_mut().expect("首次调用后宿主必须存在");
            host.child.kill().expect("kill 宿主");
            host.child.wait().expect("回收宿主");
        }
        let out = engine
            .execute(
                &utf8_prefixed("Write-Output 'kynoptic-alive-2'"),
                Duration::from_secs(30),
            )
            .expect("重生后调用不允许 Err")
            .expect("重生后必须拿到数据");
        assert_eq!(out.trim(), "kynoptic-alive-2");
        assert!(engine.host.is_some(), "重生后的宿主必须就位");
    }

    /// 宿主挂死：本次按无数据处理、降级计数 +1；下次调用惰性重生并恢复出数。
    #[cfg(windows)]
    #[test]
    fn host_hang_degrades_and_recovers() {
        let mut engine = PsEngine::production();
        let before = crate::monitors::MONITOR_DEGRADED.load(Ordering::Relaxed);
        let out = engine.execute("Start-Sleep 30", Duration::from_millis(1200));
        assert_eq!(out, Ok(None), "挂死宿主的本次调用必须按无数据处理");
        let after = crate::monitors::MONITOR_DEGRADED.load(Ordering::Relaxed);
        assert!(after > before, "挂死必须计入 MONITOR_DEGRADED");
        let out = engine
            .execute(
                &utf8_prefixed("Write-Output 'recovered'"),
                Duration::from_secs(30),
            )
            .expect("恢复调用不允许 Err")
            .expect("重生后必须拿到数据");
        assert_eq!(out.trim(), "recovered");
    }

    /// 宿主启动失败：永久回退单次 spawn 且后续调用继续可用。
    #[cfg(windows)]
    #[test]
    fn host_spawn_failure_falls_back_permanently() {
        let mut engine = no_host_engine();
        let out = engine
            .execute(
                &utf8_prefixed("Write-Output 'fallback-ok'"),
                Duration::from_secs(30),
            )
            .expect("回退后调用不允许 Err")
            .expect("回退路径必须拿到数据");
        assert_eq!(out.trim(), "fallback-ok");
        assert!(engine.permanent_fallback, "启动失败必须置永久回退标记");
        assert!(engine.host.is_none(), "回退模式下不允许残留宿主");
        let out = engine
            .execute(
                &utf8_prefixed("Write-Output 'fallback-ok-2'"),
                Duration::from_secs(30),
            )
            .expect("后续调用不允许 Err")
            .expect("回退是永久的，后续必须继续出数");
        assert_eq!(out.trim(), "fallback-ok-2");
    }

    /// 宿主与回退都启动失败：返回 Err（依赖性故障，供三态折叠为 LaunchFailed）。
    #[cfg(windows)]
    #[test]
    fn both_paths_unavailable_returns_err() {
        let mut engine = fully_broken_engine();
        let res = engine.execute(&utf8_prefixed("Write-Output 'x'"), Duration::from_secs(10));
        assert!(res.is_err(), "powershell 完全不可用时必须返回 Err");
        assert!(engine.permanent_fallback, "宿主启动失败必须置永久回退标记");
    }

    /// UTF-8 语义：中文脚本字面量与中文输出在宿主/回退两条路径都必须逐字保留。
    #[cfg(windows)]
    #[test]
    fn utf8_output_preserved_on_both_paths() {
        let script = utf8_prefixed("Write-Output '监控器降级中文测试-连接狀態'");
        let mut host_engine = PsEngine::production();
        let out = host_engine
            .execute(&script, Duration::from_secs(30))
            .expect("宿主路径不允许 Err")
            .expect("宿主路径必须拿到数据");
        assert!(
            out.contains("监控器降级中文测试-连接狀態"),
            "宿主路径中文丢失: {out:?}"
        );
        assert!(!out.contains('\u{FFFD}'), "宿主路径不允许乱码替换符");
        let mut fallback_engine = no_host_engine();
        let out = fallback_engine
            .execute(&script, Duration::from_secs(30))
            .expect("回退路径不允许 Err")
            .expect("回退路径必须拿到数据");
        assert!(
            out.contains("监控器降级中文测试-连接狀態"),
            "回退路径中文丢失: {out:?}"
        );
        assert!(!out.contains('\u{FFFD}'), "回退路径不允许乱码替换符");
    }

    /// 哨兵定界与截取：哨兵行剥离 + 空行分隔符剥离（含脚本无尾随换行的形态）。
    #[test]
    fn sentinel_cut_variants() {
        let s = b"__S__";
        // 常规：脚本输出带尾随换行
        assert_eq!(cut_before_sentinel(b"A\r\n\r\n__S__\r\n", s), "A\r\n");
        // 脚本无尾随换行：空行分隔符恰好补位
        assert_eq!(cut_before_sentinel(b"A\r\n__S__\r\n", s), "A");
        // 裸 LF 输出
        assert_eq!(cut_before_sentinel(b"A\n\r\n__S__\r\n", s), "A\n");
        // 纯脚本空输出
        assert_eq!(cut_before_sentinel(b"\r\n__S__\r\n", s), "");
        // 哨兵未出现（防御形态，协议上不可达）：截到尾部并去尾换行
        assert_eq!(cut_before_sentinel(b"AB\r\n", s), "AB");
    }

    /// base64 编码：RFC 4648 测试向量。
    #[test]
    fn base64_known_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }
}
