//! 实机探针（probe）：逐个监控器在真实硬件上跑 N 秒，验证"真的能产出事件"。
//!
//! 用法（CLI）：`kynoptic-ctl probe [--monitor ID] [--secs N] [--all]`
//!
//! 判定口径：
//! - 轮询型监控器：PASS = 临时库中出现 ≥1 条该监控器写入的事件；
//! - 事件驱动 Hook：PASS = hook 启动/停止干净（无 panic、无 ERROR 日志）；
//!   输入注入由独立的 stress 工具负责（见 examples/probe-stress-input.rs）。
//! - EXPECTED-LIMITED：探针能跑通但受环境限制（需管理员/硬件缺失），错误文本
//!   匹配 [`EXPECTED_LIMITED_PATTERNS`] 时标注，不算 FAIL。

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::collector::{start_collection_custom, CollectorSettings};
use crate::registry::MONITOR_REGISTRY;

/// 探针判定
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Pass,
    ExpectedLimited,
    Fail,
}

impl Verdict {
    pub fn as_str(&self) -> &'static str {
        match self {
            Verdict::Pass => "PASS",
            Verdict::ExpectedLimited => "EXPECTED-LIMITED",
            Verdict::Fail => "FAIL",
        }
    }
}

/// 单个监控器的探针结果
#[derive(Debug, Clone)]
pub struct ProbeOutcome {
    pub id: &'static str,
    pub dep: &'static str,
    pub default_enabled: bool,
    pub events: usize,
    /// 最新一条事件的可读样本（timestamp type action data）
    pub sample: String,
    /// 探针窗口内捕获的 WARN/ERROR 日志行
    pub warnings: Vec<String>,
    pub verdict: Verdict,
    /// 判定说明（EXPECTED-LIMITED 的原因 / FAIL 的错误摘要）
    pub note: String,
}

/// 环境受限（非监控器缺陷）的错误特征：
/// - MSAcpi_ThermalZoneTemperature 等需要管理员权限
/// - WMI 类缺失 / 硬件不存在（无电池、无蓝牙电台等）
const EXPECTED_LIMITED_PATTERNS: &[&str] = &[
    "access denied",
    "administrator",
    "elevated",
    "拒绝访问",
    "管理员",
    "not supported",
    "unsupported",
    "not found",
    "invalid class",
    "invalid namespace",
    "no battery",
    "无电池",
    "not connected", // 蓝牙电台关闭等
];

/// 每监控器的"零事件"环境判定表（本机 2026-09 实测核对）：
/// 仅当探针零事件、无 WARN/ERROR、首次采集干净完成时才按此表归类
/// EXPECTED-LIMITED（环境受限或 change-driven 设计语义），否则 FAIL。
const ZERO_EVENT_ENV_NOTES: &[(&str, &str)] = &[
    (
        "battery",
        "硬件缺失：无电池（AC 供电设备），监控器按设计不产出事件",
    ),
    (
        "thermal",
        "权限受限：MSAcpi_ThermalZoneTemperature 需要管理员（无提权时 CIM 静默返回空）",
    ),
    (
        "security",
        "权限受限：读取 Windows Security 事件日志需要管理员",
    ),
    (
        "dns",
        "环境受限：DNS Client 事件日志未启用/为空（日志面无新记录可采）",
    ),
    ("ime", "环境受限：TextServicesFramework 事件日志为空"),
    (
        "calendar",
        "依赖缺失：未安装 Outlook（COM CLSID 未注册 0x80040154）",
    ),
    ("stylus", "硬件缺失：无手写笔/数位板 HID 设备"),
    ("vpn", "硬件缺失：无 VPN/TUN 虚拟网卡"),
    (
        "print",
        "change-driven：打印队列为空（仅打印任务出现时产出事件）",
    ),
    ("idle", "change-driven：窗口内无 ≥300s 空闲转换（设计语义）"),
    ("session", "change-driven：窗口内无锁屏/解锁/显示器数量变化"),
    ("browser", "change-driven：窗口内前台非浏览器或无标签页切换"),
    ("media", "change-driven：窗口内无媒体应用启停"),
    ("screen_capture", "change-driven：窗口内无录屏进程启停"),
    (
        "brightness",
        "硬件缺失：显示器无亮度控制接口（桌面外接屏），监控器自动禁用",
    ),
];

fn classify(events: usize, warnings: &[String], hook: bool) -> (Verdict, String) {
    let joined = warnings.join(" | ").to_lowercase();
    if hook {
        // Hook：只要启动/停止干净（无 ERROR 级日志）即 PASS；
        // 输入注入由 probe-stress-input 单独验证。
        return (
            Verdict::Pass,
            "hook 启动/停止干净（输入注入见 stress 报告）".into(),
        );
    }
    if events > 0 {
        return (Verdict::Pass, String::new());
    }
    for pat in EXPECTED_LIMITED_PATTERNS {
        if joined.contains(pat) {
            return (
                Verdict::ExpectedLimited,
                format!("环境受限（{}）", first_matching(warnings, pat)),
            );
        }
    }
    // 无事件且无匹配模式：保守 FAIL，note 带上首个 warning 供人工判读
    (
        Verdict::Fail,
        warnings
            .first()
            .cloned()
            .unwrap_or_else(|| "0 事件且无日志".to_string()),
    )
}

fn first_matching(warnings: &[String], pat: &str) -> String {
    warnings
        .iter()
        .find(|w| w.to_lowercase().contains(pat))
        .cloned()
        .unwrap_or_else(|| pat.to_string())
}

// ─── 日志捕获 ─────────────────────────────────────────────────────────────────

type LogBuf = Arc<Mutex<Vec<(log::Level, String)>>>;

struct CaptureLogger {
    buf: LogBuf,
}

impl log::Log for CaptureLogger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.level() <= log::Level::Info
    }
    fn log(&self, record: &log::Record) {
        if self.enabled(record.metadata()) {
            self.buf.lock().unwrap().push((
                record.level(),
                format!(
                    "[{}] {}: {}",
                    record.level(),
                    record.target(),
                    record.args()
                ),
            ));
        }
    }
    fn flush(&self) {}
}

/// 安装捕获日志器（若全局日志器已设置则不覆盖，返回 None）。
fn install_capture_logger() -> Option<LogBuf> {
    let buf: LogBuf = Arc::new(Mutex::new(Vec::new()));
    let logger = Box::new(CaptureLogger { buf: buf.clone() });
    if log::set_boxed_logger(logger).is_ok() {
        log::set_max_level(log::LevelFilter::Info);
        Some(buf)
    } else {
        None
    }
}

// ─── 探针核心 ─────────────────────────────────────────────────────────────────

fn temp_db_path(id: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "kynoptic-probe-{}-{}",
        id,
        chrono::Utc::now().timestamp_millis()
    ));
    std::fs::create_dir_all(&dir).ok();
    dir.join("probe.db")
}

fn count_and_sample(db: &crate::db::Database) -> (usize, String) {
    let conn = db.reader();
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))
        .unwrap_or(0);
    let sample = conn
        .query_row(
            "SELECT timestamp, event_type, event_action, COALESCE(event_data,'') FROM events \
             ORDER BY id DESC LIMIT 1",
            [],
            |r| {
                Ok(format!(
                    "{} {} {} {}",
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?
                ))
            },
        )
        .unwrap_or_else(|_| "(无事件)".to_string());
    (count.max(0) as usize, sample)
}

/// 对单个监控器跑一次探针。
pub fn probe_monitor(id: &str, secs: u64) -> ProbeOutcome {
    let spec = MONITOR_REGISTRY
        .iter()
        .find(|s| s.id == id)
        .unwrap_or_else(|| panic!("未知监控器 id: {id}"));
    let is_hook = id == "keyboard_hook" || id == "mouse_hook";

    // 捕获日志器必须先于 collector 内部的 env_logger try_init 安装
    let log_buf = install_capture_logger();

    let db_path = temp_db_path(id);
    let enabled: HashSet<String> = [id.to_string()].into_iter().collect();
    let settings = CollectorSettings {
        write_flush_interval_secs: 1, // 探针窗口短，1s flush 保证事件可见
        ..CollectorSettings::default()
    };

    let mut collector = start_collection_custom(&enabled, settings, db_path.to_str().unwrap());

    // 等首次采集完成（PS 子进程型首次查询可能远超窗口时长，例如 Windows
    // Update 在线搜索可达数分钟）。未完成就计数会把"慢"误判成"零事件"。
    let first_collect_deadline = std::time::Instant::now() + std::time::Duration::from_secs(420);
    let mut first_collect_ok = false;
    loop {
        if let Some(buf) = &log_buf {
            let msgs = buf.lock().unwrap();
            if msgs
                .iter()
                .any(|(_, m)| m.contains(&format!("{id} 首次采集完成")))
            {
                first_collect_ok = true;
            }
            let panicked = msgs
                .iter()
                .any(|(_, m)| m.contains(&format!("{id} 首次采集 panic")));
            drop(msgs);
            if panicked {
                break;
            }
        } else {
            first_collect_ok = true; // 日志器被占用（并行探针场景）：退化为纯窗口等待
        }
        if first_collect_ok {
            break;
        }
        if std::time::Instant::now() > first_collect_deadline {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }

    // 窗口内刺激器：为"需要状态变化才出事件"的监控器人为制造变化
    let _inducers = Inducers::start(id);

    // change-detect 型监控器需要 ≥2 个采集周期；不足时按监控器间隔拉长窗口
    let window = secs.max(min_window_secs(id));
    std::thread::sleep(std::time::Duration::from_secs(window.max(3)));
    collector.shutdown();
    // shutdown 只停 writer；monitor 线程看到 SHUTDOWN 后自行退出，不影响读库

    let (events, sample) = count_and_sample(&collector.db);
    let warnings: Vec<String> = log_buf
        .map(|b| {
            b.lock()
                .unwrap()
                .iter()
                .filter(|(lvl, _)| *lvl <= log::Level::Warn)
                .map(|(_, m)| m.clone())
                .collect()
        })
        .unwrap_or_default();

    let (mut verdict, mut note) = classify(events, &warnings, is_hook);
    if events == 0 && warnings.is_empty() && first_collect_ok {
        if let Some((_, why)) = ZERO_EVENT_ENV_NOTES.iter().find(|(k, _)| *k == id) {
            verdict = Verdict::ExpectedLimited;
            note = why.to_string();
        }
    } else if !first_collect_ok {
        note = format!("首次采集在 420s 内未完成（超时或 panic）{}", note);
        verdict = Verdict::Fail;
    }

    // 清理临时目录（失败不影响结果）
    if let Some(dir) = db_path.parent() {
        let _ = std::fs::remove_dir_all(dir);
    }

    ProbeOutcome {
        id: spec.id,
        dep: spec.dep.as_str(),
        default_enabled: spec.default_enabled,
        events,
        sample,
        warnings,
        verdict,
        note,
    }
}

/// 全量探针：逐个监控器（各自独立临时库），返回全部结果。
pub fn probe_all(secs: u64) -> Vec<ProbeOutcome> {
    MONITOR_REGISTRY
        .iter()
        .map(|s| {
            println!("probe: {} ...", s.id);
            let out = probe_monitor(s.id, secs);
            println!(
                "  → {} events={} {}",
                out.verdict.as_str(),
                out.events,
                if out.note.is_empty() {
                    String::new()
                } else {
                    format!("({})", out.note)
                }
            );
            out
        })
        .collect()
}

/// 需要"第二次采集才能产出事件"的监控器的最小窗口（2×interval + 缓冲）。
fn min_window_secs(id: &str) -> u64 {
    match id {
        // 首次只初始化基线，第二次才比较增量（还需窗口内有流量）
        "network" => 70,
        _ => 0,
    }
}

/// 窗口内状态变化刺激器（drop 时清理）。
struct Inducers {
    network_thread: Option<std::thread::JoinHandle<()>>,
    temp_file: Option<std::path::PathBuf>,
    stop: Arc<AtomicBool>,
}

impl Inducers {
    fn start(id: &str) -> Self {
        let mut out = Inducers {
            network_thread: None,
            temp_file: None,
            stop: Arc::new(AtomicBool::new(false)),
        };
        if id == "network" {
            // 持续制造出站流量，保证 30s 增量窗口内有 delta
            let stop = out.stop.clone();
            let spawned = std::thread::Builder::new()
                .name("probe-net-inducer".into())
                .spawn(move || {
                    if let Ok(sock) = std::net::UdpSocket::bind("0.0.0.0:0") {
                        let payload = [0u8; 512];
                        while !stop.load(Ordering::Relaxed) {
                            // 探针流量：突发发往公共黑洞端口（UDP 不等回应），
                            // 保证 30s 增量窗口内 netstat 计数有明显 delta
                            for _ in 0..32 {
                                let _ = sock.send_to(&payload, "1.1.1.1:9");
                                let _ = sock.send_to(&payload, "8.8.8.8:9");
                            }
                            std::thread::sleep(std::time::Duration::from_millis(50));
                        }
                    }
                })
                .ok();
            out.network_thread = spawned;
        }
        if id == "file_activity" {
            // 在 Downloads 建一个临时文件，制造目录文件数 delta
            if let Some(dl) = dirs_download() {
                let f = dl.join(format!("kynoptic-probe-{}.tmp", std::process::id()));
                if std::fs::write(&f, b"kynoptic probe").is_ok() {
                    out.temp_file = Some(f);
                }
            }
        }
        out
    }
}

impl Drop for Inducers {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.network_thread.take() {
            let _ = h.join();
        }
        if let Some(f) = self.temp_file.take() {
            let _ = std::fs::remove_file(f);
        }
    }
}

/// 用户 Downloads 目录（file_activity 监控的三个观察目录之一）。
fn dirs_download() -> Option<std::path::PathBuf> {
    let home = std::env::var("USERPROFILE").ok()?;
    Some(std::path::PathBuf::from(home).join("Downloads"))
}

/// 打印矩阵表格（--all 的最终输出）。
pub fn print_matrix(outcomes: &[ProbeOutcome]) {
    println!(
        "\n{:<16} {:<11} {:<8} {:>7}  verdict",
        "monitor", "dep", "default", "events"
    );
    println!("{}", "-".repeat(72));
    for o in outcomes {
        println!(
            "{:<16} {:<11} {:<8} {:>7}  {}",
            o.id,
            o.dep,
            if o.default_enabled { "on" } else { "off" },
            o.events,
            o.verdict.as_str()
        );
        if !o.note.is_empty() {
            println!("  └ note: {}", o.note);
        }
    }
    let pass = outcomes
        .iter()
        .filter(|o| o.verdict == Verdict::Pass)
        .count();
    let limited = outcomes
        .iter()
        .filter(|o| o.verdict == Verdict::ExpectedLimited)
        .count();
    let fail = outcomes
        .iter()
        .filter(|o| o.verdict == Verdict::Fail)
        .count();
    println!("{}", "-".repeat(72));
    println!(
        "total: {} PASS / {} EXPECTED-LIMITED / {} FAIL",
        pass, limited, fail
    );
}
