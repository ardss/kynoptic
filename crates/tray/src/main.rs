//! kynoptic-tray — Kynoptic 托盘壳(v0.1 桌面产品形态)
//!
//! 单进程三职责:
//! 1. 采集器:kynoptic_core::collector::start_collection(默认 14 监控器,
//!    --all 启用全集);DB 路径用 kynoptic_core::db::resolve_db_path(--db 覆盖)
//! 2. 本地 dashboard HTTP 服务(127.0.0.1,实现在 kynoptic-dash crate,与
//!    `kynoptic-ctl dashboard` 共用唯一代码,只读打开)
//! 3. 系统托盘图标(Shell_NotifyIconW)+ 五项右键菜单,无主窗口,仅消息循环
//!
//! 极致轻量铁律:纯 Win32 API(windows-sys)+ std::net,无 Tauri/WinUI3/web 框架,
//! 无通知气泡,无合成输入注入。
//!
//! 暂停语义:Pause 仅停采集(置停机旗标 + Collector::shutdown 内 join writer),
//! Resume 重新 start_collection;DB 与 dashboard 服务不重启。
//!
//! # 物理验证步骤(自动化 shell 里托盘 UI 不可见,属预期;需人工冒烟)
//! 1. 双击 target/debug/kynoptic-tray.exe(或 cargo run -p kynoptic-tray)
//! 2. 系统托盘出现绿色实心圆图标(hover 提示 "Kynoptic: collecting")
//! 3. 右键菜单五项可用:Open Dashboard / Pause / Open data folder / 分隔线 / Quit
//!    - Open Dashboard 打开 http://127.0.0.1:8422 面板
//!    - Pause 后图标变灰色空心圆;Resume 变回绿色实心圆
//!    - Open data folder 打开资源管理器并定位 DB 目录
//!    - Quit 后进程退出、托盘图标消失、任务管理器确认 RSS < 5MB
//!
//! 用法:kynoptic-tray [--db PATH] [--port N] [--all]

mod args;
mod filelog;
mod ghost;
mod icons;
mod paths;
mod state;
mod tray;

use std::sync::mpsc;
use std::thread;

use tray::CollectorCmd;

/// 采集器运行旗标（审查 P1）：采集器属主线程在 start_collection 成功后置
/// true,Pause/Quit/启动失败时复位 false;心跳线程据此决定是否计算 stalled。
/// 静态量理由:心跳线程拿不到 owner 线程栈上的 Collector 实例（所有权不跨
/// 线程）,与 core 的 LAST_FLUSH_EPOCH 同属进程级共享信号。
static COLLECTOR_RUNNING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// dashboard 服务健康旗标（0=未定,1=失败,2=正常）。dash 线程写，托盘 UI
/// 定时读——Error 态图标此前是死代码，现在真正接线（定性审查）。
static DASH_FAILED: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);
/// 采集器故障旗标（审查 P1）：启动失败（DB 损坏/被锁/磁盘满/启用集为空）置
/// true，成功启动清零。托盘 UI 定时读它切 Error 图标——此前采集器故障没有
/// 任何静态量接入 UI，图标保持绿色"采集中"，数据静默归零。
pub static COLLECTOR_FAILED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// 采集停滞阈值（审查 P1）：采集器在跑且"醒着的时间"内 flush 停滞超过该秒数,
/// 心跳打 stalled:true。30s 写一轮心跳、正常批次间隔远小于此。
const HEARTBEAT_STALLED_SECS: i64 = 1800; // 30min：必须大于最慢的周期性写入者
                                          //（process 监控器 600s 强制心跳 + 30% 抖动 ≈ 780s），否则空闲机器会被误判
                                          // stalled 遭看门狗循环误杀（全库审查 P0：300<600 的余量倒挂）

/// 进程启动时刻的无偏中断时间基线（秒）。QueryUnbiasedInterruptTime 不计
/// 系统休眠时间，是区分"机器睡过"与"writer 挂死"的唯一可靠时钟——墙钟 gap
/// 在休眠唤醒后第一拍必然巨大，且旧实现那个 >4h 才豁免的护栏挡不住
/// 30min-4h 的睡眠（唤醒后 healthy 托盘被 watchdog 误杀的根因）。
static PROCESS_START_UNBIASED_SECS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
/// 最近一次观测到 flush 前进时的无偏秒数（心跳线程刷新）。stalled 判据 =
/// 当前无偏秒 - 该值 > HEARTBEAT_STALLED_SECS；休眠不积累无偏时间，
/// 所以睡着的机器永远不会因此被打 stalled。
static LAST_FLUSH_AWAKE_SECS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

// QueryUnbiasedInterruptTime（100ns 计数,不含休眠时间）。tray 的
// windows-sys 未启用 Win32_System_SystemInformation feature，按 session.rs
// 的先例手写 extern 声明，避免为单个函数动依赖表。
extern "system" {
    fn QueryUnbiasedInterruptTime(lpUnbiasedTime: *mut u64) -> i32;
}

/// 进程累计醒着的秒数（无偏中断时间 - 启动基线；休眠期间不增长）。
fn awake_secs() -> u64 {
    let mut t: u64 = 0;
    let ok = unsafe { QueryUnbiasedInterruptTime(&mut t) };
    if ok == 0 {
        return 0;
    }
    (t / 10_000_000)
        .saturating_sub(PROCESS_START_UNBIASED_SECS.load(std::sync::atomic::Ordering::Relaxed))
}

fn main() {
    // 心跳基线先行：QueryUnbiasedInterruptTime 按进程启动时刻取样
    let mut unbiased_raw: u64 = 0;
    unsafe {
        QueryUnbiasedInterruptTime(&mut unbiased_raw);
    }
    PROCESS_START_UNBIASED_SECS.store(
        unbiased_raw / 10_000_000,
        std::sync::atomic::Ordering::Relaxed,
    );

    let argv: Vec<String> = std::env::args().skip(1).collect();
    let port_from_args = argv.iter().any(|a| a == "--port");
    let mut parsed = match args::parse(&argv) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("kynoptic-tray: {e}");
            std::process::exit(2);
        }
    };

    // Wave20 P1：settings.json 的 dashboard_port 此前是死字段——无 --port
    // 时以设置值为首选端口（仍走既有候选回退；改动需重启托盘生效，设置
    // 页已有提示文案）。
    if !port_from_args {
        let st = kynoptic_dash::settings::load(&parsed.db);
        if st.dashboard_port != parsed.port {
            parsed.port = st.dashboard_port;
        }
    }

    // 文件 logger（日志全景审查 P0）：必须先于一切 log::* 调用安装——
    // 此前托盘进程从未初始化 logger，main.rs/ghost.rs/dash 里的 warn!/error!
    // 全部静默丢弃，违反"后台错误必须落文件"铁律。写 <db 目录>\tray.log
    // （1MB 轮转 .old）；此后 collector 内部的 env_logger try_init 会静默
    // 让位，全进程日志统一落本文件。
    filelog::init(
        parsed
            .db
            .parent()
            .unwrap_or(std::path::Path::new("."))
            .join("tray.log"),
    );
    log::info!(
        "kynoptic-tray 启动: db={} port={}",
        parsed.db.display(),
        parsed.port
    );

    // 单实例互斥体:防双开,同时是 watchdog 的存活探针
    {
        use windows_sys::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
        use windows_sys::Win32::System::Threading::CreateMutexW;
        let name: Vec<u16> = format!("{}\0", paths::single_instance_mutex_name())
            .encode_utf16()
            .collect();
        unsafe {
            // 只在句柄非空时才看 GetLastError:创建成功的新互斥体不重置
            // last error,残留的 ERROR_ALREADY_EXISTS 会造成误判秒退。
            let h = CreateMutexW(std::ptr::null(), 0, name.as_ptr());
            // fail-closed(审查 P2):CreateMutexW 返回 NULL 说明互斥体没建起来,
            // 若继续跑,watchdog 的 OpenMutexW 探活失效 + 双 collector 可能并发
            // 写同一 SQLite。报错退出(stderr + 日志文件)而不是静默继续。
            if h.is_null() {
                let msg = format!(
                    "[{}] kynoptic-tray: CreateMutexW 失败(GetLastError={}),拒绝启动以防双 collector 并发写库\n",
                    chrono::Utc::now().to_rfc3339(),
                    GetLastError()
                );
                eprint!("{msg}");
                let log_path = std::env::current_exe()
                    .ok()
                    .and_then(|e| e.parent().map(|d| d.join("tray-error.log")));
                if let Some(p) = log_path {
                    let _ = std::fs::write(&p, &msg);
                }
                std::process::exit(1);
            }
            if GetLastError() == ERROR_ALREADY_EXISTS {
                // 留痕（审查：自启动场景无 console，eprintln 会被丢弃；
                // filelog 已初始化，warn 落 tray.log 供用户自助定位）
                log::warn!("已有实例在运行(互斥体已存在)，本实例退出");
                eprintln!("kynoptic-tray: 已有实例在运行,退出");
                return;
            }
            // 升级过渡桥：同时持有旧名互斥体——旧版 tray/collect 只持/只探
            // 旧名，不持旧名则升级窗口里新旧实例互不可见（单实例保护失效）。
            // 旧版实例尚在本会话运行时此处报已存在，同样退出。
            if let Some(legacy) = kynoptic_core::singleton::legacy_bridge_mutex_name(
                &paths::single_instance_mutex_name(),
            ) {
                let lname: Vec<u16> = format!("{legacy}\0").encode_utf16().collect();
                let lh = CreateMutexW(std::ptr::null(), 0, lname.as_ptr());
                if lh.is_null() {
                    log::warn!("过渡桥互斥体创建失败(GetLastError={})", GetLastError());
                } else if GetLastError() == ERROR_ALREADY_EXISTS {
                    log::warn!("已有旧版实例在运行(过渡桥互斥体已存在)，本实例退出");
                    eprintln!("kynoptic-tray: 已有实例在运行,退出");
                    return;
                }
            }
        }
    }

    // 启动即清"用户主动退出"旗标:之后 watchdog 才有拉起依据。
    // 路径规则与 watchdog 统一:--flag 覆盖 > KYNOPTIC_EXIT_FLAG > exe 同目录
    // (契约见 paths.rs 模块注释;watchdog 侧在 crates/cli/src/main.rs)。
    let exit_flag = parsed
        .exit_flag
        .clone()
        .unwrap_or_else(paths::resolve_exit_flag);
    let _ = std::fs::remove_file(&exit_flag);

    // 幽灵 session 清扫（双 open 修复）：在采集器 open DB 之前闭合全部遗留
    // open session。必须先于 CollectorCmd::Start——采集器启动路径里的
    // close_ghost_sessions 会把"最新的 1 个幽灵"当当前 session 保留，随后
    // start_session 再建一个，重启后永远双 open（见 ghost.rs 模块注释）。
    if parsed.db.exists() {
        // 审查 P2：清扫失败（DB 被锁/损坏）与"没有幽灵"必须区分——Err 时
        // 留痕告警（托盘无 console，log 走文件 + stderr 兜底），不阻塞启动。
        if let Err(e) = ghost::close_all_open_sessions(&parsed.db) {
            log::warn!("启动幽灵 session 清扫失败: {e}");
            eprintln!("kynoptic-tray: 启动幽灵 session 清扫失败: {e}");
        }
    }

    // dashboard 服务线程:与采集器同生命周期;只读打开,失败仅记录不阻塞托盘。
    // 健壮性:全新首装时本线程先于采集器跑,数据库文件还不存在,只读打开
    // 必失败且托盘无控制台（错误不可见,外面就是"拒绝连接"）。因此先等库
    // 文件就绪（至多 60s）,serve 失败再写日志文件,绝不静默消失。
    //
    // P1 端口回退重构（Hyper-V/WSL excludedportrange 实测覆盖 8408-8507,
    // 旧实现启动即 pick、60s 后才 bind,候选又全落在保留区 → os error 10013
    // 面板必死）:
    //  (a) pick 挪进本线程、紧贴 serve 的 bind 之前（TOCTOU 窗口从 60s 压到
    //      毫秒级）;
    //  (b) bind 失败按 args::candidate_ports 的候选序列（三段跨 +10000 大步长）
    //      继续重试,不再首选段全灭即放弃;
    //  (c) dashboard-port.txt 只在真正 bind 成功后写;全部候选失败时写
    //      "unavailable" 并把错误追加到 dashboard-error.log。
    let dash_db = parsed.db.clone();
    let dash_port_requested = parsed.port;
    // 实际绑定端口回传主线程（托盘菜单 Open Dashboard 用同一端口,不打开死链）
    let (port_tx, port_rx) = mpsc::channel::<Option<u16>>();
    // spawn 失败降级（审查 P1：.expect 会 panic 全进程且发生在收尾之前，
    // exit 旗标写不出去，watchdog 持续拉起形成崩溃-重启循环）——dashboard
    // 失败可降级：记日志、置失败旗标，托盘与采集器继续活着。
    let _dash_handle = thread::Builder::new()
        .name("Dashboard".into())
        .spawn(move || {
            for _ in 0..120 {
                if dash_db.exists() {
                    break;
                }
                thread::sleep(std::time::Duration::from_millis(500));
            }
            // port 0（随机空闲端口）语义保留,不走候选回退。
            let candidates: Vec<u16> = if dash_port_requested == 0 {
                vec![0]
            } else {
                args::candidate_ports(dash_port_requested)
            };
            let mut last_err: Option<String> = None;
            for &cand in &candidates {
                // 探测 bind 紧贴 serve:成功即写 port.txt 并回传端口,再交 serve
                // 正式 bind（探测 listener 立即 drop,毫秒级窗口;单实例互斥体
                // 已排除第二个 kynoptic 抢端口）。serve 失败（含罕见 TOCTOU
                // 撞车）按候选序列继续重试。
                match std::net::TcpListener::bind(("127.0.0.1", cand)) {
                    Ok(probe) => drop(probe),
                    Err(e) => {
                        last_err = Some(format!("bind 127.0.0.1:{cand}: {e}"));
                        continue;
                    }
                }
                if cand != 0 {
                    if let Some(dir) = dash_db.parent() {
                        let _ = std::fs::write(dir.join("dashboard-port.txt"), format!("{cand}\n"));
                    }
                }
                DASH_FAILED.store(2, std::sync::atomic::Ordering::Relaxed);
                let _ = port_tx.send(Some(cand));
                // catch_unwind：serve panic 不许让线程静默死亡还挂着绿色
                // Running 图标（审查 P2：活着但残废）——转成 Err 走候选重试
                let attempt = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    kynoptic_dash::serve(&dash_db, cand, true)
                }));
                let result = match attempt {
                    Ok(r) => r,
                    Err(_) => Err(kynoptic_core::Error::InvalidData(
                        "dashboard 线程 panic".into(),
                    )),
                };
                match result {
                    Ok(()) => return, // 正常退出路径（进程结束）
                    Err(e) => {
                        last_err = Some(format!("serve 127.0.0.1:{cand}: {e}"));
                        continue;
                    }
                }
            }
            // 全部候选失败:port.txt 写 "unavailable",错误留档 dashboard-error.log
            DASH_FAILED.store(1, std::sync::atomic::Ordering::Relaxed);
            if let Some(dir) = dash_db.parent() {
                let _ = std::fs::write(dir.join("dashboard-port.txt"), "unavailable\n");
                if let Some(err) = last_err {
                    let msg = format!(
                        "[{}] dashboard 所有候选端口绑定失败: {err}\n",
                        chrono::Utc::now().to_rfc3339()
                    );
                    eprint!("{msg}");
                    use std::io::Write;
                    if let Ok(mut f) = std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(dir.join("dashboard-error.log"))
                    {
                        let _ = f.write_all(msg.as_bytes());
                    }
                }
            }
            let _ = port_tx.send(None);
        });
    if let Err(e) = &_dash_handle {
        log::error!("dashboard 线程启动失败（面板不可用，采集不受影响）: {e}");
        DASH_FAILED.store(1, std::sync::atomic::Ordering::Relaxed);
    }

    // 等实际端口（探测 bind 就绪即回传,常见路径毫秒级;库文件缺失的冷启动
    // 路径最多等 2s,超时则菜单退回请求端口——与旧行为一致,不阻塞托盘出现）。
    match port_rx.recv_timeout(std::time::Duration::from_secs(2)) {
        Ok(Some(p)) => {
            parsed.port = p;
            if p != dash_port_requested && dash_port_requested != 0 {
                let msg = format!(
                    "面板已换端口：{dash_port_requested} 不可用，dashboard 改用 {p}（http://127.0.0.1:{p}，已写入 dashboard-port.txt）"
                );
                log::warn!("{msg}");
                eprintln!("kynoptic-tray: {msg}");
            }
        }
        _ => {
            parsed.port = dash_port_requested;
        }
    }

    // 采集心跳:每 30s touch exe 同目录心跳文件。
    // watchdog 除互斥体探活外还会检查心跳新鲜度——进程活着但采集主循环挂死
    // 时,心跳停止,watchdog 据此 kill 并重启(4-8 小时空洞的根因修复)。
    // 写失败静默忽略:心跳缺失只是退化为旧的探活行为,不影响采集本身。
    //
    // 审查 P1 心跳解耦修复:旧实现心跳线程无脑每 30s 写 RFC3339,采集主循环
    // 挂死时心跳照常新鲜,"心跳新鲜 = 采集健康"被架空。现在内容升级为 JSON
    // {"pid","ts","flush","stalled"}:flush 取 core 的 last_flush_epoch()(writer
    // 每次成功落库刷新);采集运行中且 flush 停滞超 HEARTBEAT_STALLED_SECS(1800s)
    // 时打 stalled:true,watchdog 侧 classify_heartbeat 据此判为过期并 kill。
    // 旧版纯时间戳内容仍被 watchdog 兼容解析(向后兼容,滚动升级期两代共存)。
    //
    // 休眠误报修复（P1）：stalled 判据从墙钟 gap 改为"无偏秒差"——JSON 里新增
    // "awake" 字段（进程累计醒着秒数,QueryUnbiasedInterruptTime 不计休眠），
    // stalled = flush 前进之后醒着超过 1800s 没再前进。机器睡着时无偏时间
    // 停走,唤醒后第一拍 awake 差值接近 0,绝不会被误报;watchdog 侧只认
    // stalled 布尔位,旧字段全部保留,新旧两代可共存。
    {
        let hb = paths::resolve_heartbeat();
        // 心跳写失败留档路径（数据目录旁,与采集器错误日志同一文件）;
        // 数据目录也无写权限时留档本身失败,尽力而为。
        let hb_err_log = parsed.db.parent().map(|p| p.join("collector-error.log"));
        let hb_handle = thread::Builder::new()
            .name("Heartbeat".into())
            .spawn(move || {
                let mut prev_flush: u64 = 0;
                // 心跳写失败计数（磁盘满/exe 目录 ACL 锁死/attrib +R/卷只读）:
                // 旧实现 `let _` 静默吞掉 → 心跳 mtime 冻结 → watchdog 判挂死
                // 强杀健康托盘 → 新托盘依旧写不出 → 无限 kill/重启循环,每轮
                // 丢通道内未 flush 事件。现改为:失败计数随下一拍写入心跳 JSON
                // （unwritable/wfail 字段,恢复后首个成功写入会把旗标落盘）,
                // watchdog 见旗标改判环境故障只告警不 kill;并留档
                // collector-error.log（首次失败 + 之后每 20 次,避免刷盘）。
                let mut write_failures: u64 = 0;
                loop {
                    let now = chrono::Utc::now();
                    let flush = kynoptic_core::collector::last_flush_epoch();
                    let running = COLLECTOR_RUNNING.load(std::sync::atomic::Ordering::Relaxed);
                    let awake = awake_secs();
                    // flush==0 = 本进程尚未落过库(启动初期正常),不误报;真正
                    // 挂死场景是 flush 曾前进后停滞,由 1800s 阈值覆盖。
                    // flush 前进即刷新"最近活跃时刻"的无偏基线。
                    if flush > 0 && flush != prev_flush {
                        LAST_FLUSH_AWAKE_SECS.store(awake, std::sync::atomic::Ordering::Relaxed);
                        prev_flush = flush;
                    }
                    // 醒着的时间里 flush 停滞超阈值才报 stalled（休眠不积累醒着
                    // 时间,唤醒后的巨大墙钟 gap 在此归零,无需任何护栏豁免）
                    let stalled = running
                        && flush > 0
                        && awake.saturating_sub(
                            LAST_FLUSH_AWAKE_SECS.load(std::sync::atomic::Ordering::Relaxed),
                        ) > HEARTBEAT_STALLED_SECS as u64;
                    // unwritable/wfail 反映的是"上一拍"的写结果:本拍若也失败,
                    // 旗标留在内存、文件保持旧内容,恢复后随成功写入落盘。
                    let content = format!(
                        "{{\"pid\":{},\"ts\":\"{}\",\"flush\":{},\"awake\":{},\"stalled\":{},\"unwritable\":{},\"wfail\":{}}}",
                        std::process::id(),
                        now.to_rfc3339(),
                        flush,
                        awake,
                        stalled,
                        write_failures > 0,
                        write_failures
                    );
                    if let Err(e) = std::fs::write(&hb, &content) {
                        write_failures += 1;
                        if write_failures == 1 || write_failures.is_multiple_of(20) {
                            log::error!("心跳文件写入失败(连续 {write_failures} 次): {e}");
                            if let Some(err_path) = &hb_err_log {
                                use std::io::Write as _;
                                if let Ok(mut f) = std::fs::OpenOptions::new()
                                    .create(true)
                                    .append(true)
                                    .open(err_path)
                                {
                                    let _ = writeln!(
                                        f,
                                        "[{}] 心跳文件写入失败(连续 {write_failures} 次): {e}（watchdog 将仅告警不 kill）",
                                        chrono::Local::now().format("%Y-%m-%d %H:%M:%S")
                                    );
                                }
                            }
                        }
                    } else {
                        write_failures = 0;
                    }
                    thread::sleep(std::time::Duration::from_secs(30));
                }
            });
        // spawn 失败降级（审查 P1）：心跳缺失会让 watchdog 退化为纯互斥体
        // 探活，不能因此 panic 全进程（panic 发生在 exit 旗标写出之前，
        // 会形成崩溃-重启循环）
        if let Err(e) = &hb_handle {
            log::error!("心跳线程启动失败（watchdog 退化为互斥体探活）: {e}");
        }
    }

    // 自动更新检查（用户设计要求：更新发现必须自动，不能指望用户敲命令）：
    // 启动 2 分钟后首查，之后每 24h 一次。子进程跑 `kynoptic update --check`
    //（托盘不带 HTTP 客户端，复用 CLI 的 GitHub 查询与预发布过滤），把结果
    // 写 data\update-available.txt——托盘菜单动态插入一键更新项，dashboard
    // 状态栏同步提示。检查失败静默（网络差不该变成用户的负担）。
    {
        let db_for_upd = parsed.db.clone();
        let exe_dir = std::env::current_exe()
            .ok()
            .and_then(|e| e.parent().map(|p| p.to_path_buf()));
        let upd_handle = thread::Builder::new()
            .name("UpdateCheck".into())
            .spawn(move || {
                // Some(v)=有新版；None=明确无更新或查询失败。二者都清提示文件；
                // 查询失败（子进程/网络挂）时保留旧文件不清除（全库审查 P1：
                // 一次网络抖动不该让已发现的更新提示消失 24h）。
                enum CheckOutcome {
                    Update(String),
                    UpToDate,
                    Failed,
                }
                let run_check = || -> CheckOutcome {
                    let Some(exe_dir) = exe_dir.as_ref() else {
                        return CheckOutcome::Failed;
                    };
                    let exe = exe_dir.join("kynoptic.exe");
                    use std::os::windows::process::CommandExt;
                    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
                    // Wave17 审查 P1：必须带超时。self_update/rustls 无内置
                    // 超时，网络半开（VPN 挂起等）会让 .output() 永久阻塞，
                    // 之后 24h 周期检查全部失效。
                    let Ok(mut child) = std::process::Command::new(exe)
                        .args(["update", "--check"])
                        .stdout(std::process::Stdio::piped())
                        .stderr(std::process::Stdio::null())
                        .creation_flags(CREATE_NO_WINDOW)
                        .spawn()
                    else {
                        return CheckOutcome::Failed;
                    };
                    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
                    let mut out = None;
                    while std::time::Instant::now() < deadline {
                        match child.try_wait() {
                            Ok(Some(status)) => {
                                // 读完管道再退出，避免子进程因管道满阻塞
                                use std::io::Read;
                                let mut buf = String::new();
                                if let Some(mut io) = child.stdout.take() {
                                    let _ = io.read_to_string(&mut buf);
                                }
                                let _ = status;
                                out = Some(buf);
                                break;
                            }
                            Ok(None) => thread::sleep(std::time::Duration::from_millis(250)),
                            Err(_) => break,
                        }
                    }
                    let Some(text) = (match out {
                        Some(t) => Some(t),
                        None => {
                            // 超时/等待出错：杀掉子进程，视为查询失败
                            let _ = child.kill();
                            let _ = child.wait();
                            None
                        }
                    }) else {
                        return CheckOutcome::Failed;
                    };
                    if text.contains("UP TO DATE") {
                        return CheckOutcome::UpToDate;
                    }
                    match text
                        .lines()
                        .find(|l| l.starts_with("UPDATE "))
                        .and_then(|l| l.split_whitespace().nth(1))
                        .map(|v| v.trim_start_matches('v').to_string())
                    {
                        Some(v) => CheckOutcome::Update(v),
                        None => CheckOutcome::Failed,
                    }
                };
                let upd_path = db_for_upd
                    .parent()
                    .map(|d| d.join("update-available.txt"))
                    .unwrap_or_else(|| std::path::PathBuf::from("update-available.txt"));
                loop {
                    thread::sleep(std::time::Duration::from_secs(120)); // 启动缓冲，避开开机网络未就绪
                    match run_check() {
                        CheckOutcome::Update(v) => {
                            let _ = std::fs::write(&upd_path, &v);
                        }
                        CheckOutcome::UpToDate => {
                            let _ = std::fs::remove_file(&upd_path);
                        }
                        CheckOutcome::Failed => {
                            // 查询失败：保留已有提示文件不动
                        }
                    }
                    thread::sleep(std::time::Duration::from_secs(24 * 3600));
                }
            });
        // spawn 失败降级（审查 P1）：更新检查是可降级服务，不允许 panic 托盘
        if let Err(e) = &upd_handle {
            log::error!("更新检查线程启动失败（自动更新发现不可用）: {e}");
        }
    }

    // 采集器属主线程:Collector 只在本线程构造/持有/关停(所有权不跨线程)
    let (cmd_tx, cmd_rx) = mpsc::channel::<CollectorCmd>();
    let owner_db = parsed.db.clone();
    let owner_all = parsed.all;
    let owner = thread::Builder::new()
        .name("CollectorOwner".into())
        .spawn(move || {
            let mut collector: Option<kynoptic_core::collector::Collector> = None;
            // Pause 状态记忆（审查 P2：设置保存触发的 Start 不能解除用户的
            // 暂停——只有托盘菜单的 Resume 才解除）
            let mut paused = false;
            // 启用集为空的失败态：阻止 60s 超时空转重试（留档有界，只在
            // 设置再次变更时重新评估）
            let mut empty_set = false;
            // 审查 P0：启动失败后不能只等下一条命令才重试——改为 60s 超时
            // 醒来一次，采集器缺位且未暂停时自动重试（库被占/磁盘满恢复后
            // 自愈，无需用户干预）。超时重试走 Resume 语义：collector 为 None
            // 时 Resume 分支的"清掉上一实例"自然跳过。
            loop {
                let cmd = match cmd_rx.recv_timeout(std::time::Duration::from_secs(60)) {
                    Ok(c) => c,
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        if collector.is_none() && !paused && !empty_set {
                            CollectorCmd::Resume
                        } else {
                            continue;
                        }
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                };
                match cmd {
                    CollectorCmd::Start | CollectorCmd::Resume => {
                        if matches!(cmd, CollectorCmd::Resume) {
                            paused = false;
                        }
                        if paused {
                            log::info!("Paused：设置变更已记录，恢复采集后生效");
                            continue;
                        }
                        // Resume/重启前先清掉上一实例(如有)
                        if let Some(c) = collector.as_mut() {
                            c.shutdown();
                        }
                        // 每次启动都读 settings.json（dashboard 保存即写此处），
                        // 使"设置页勾选 = 实际采集集"单一事实源；--all 仍可覆盖。
                        let app_settings = kynoptic_dash::settings::load(&owner_db);
                        let enabled: std::collections::HashSet<String> = if owner_all {
                            kynoptic_core::registry::all_monitor_ids()
                                .into_iter()
                                .map(String::from)
                                .collect()
                        } else {
                            app_settings
                                .enabled_monitors
                                .iter()
                                .filter(|id| {
                                    kynoptic_core::registry::all_monitor_ids()
                                        .contains(&id.as_str())
                                })
                                .cloned()
                                .collect()
                        };
                        let granularity = if app_settings.input_counts_only {
                            kynoptic_core::collector::InputGranularity::Minute
                        } else {
                            kynoptic_core::collector::InputGranularity::Raw
                        };
                        let cs = kynoptic_core::collector::CollectorSettings {
                            input_granularity: granularity,
                            // vk 频次开关：settings.json 的 vk_frequency_enabled
                            // （serde 默认 false）→ CollectorSettings → core 的
                            // input_agg::set_vk_enabled（collector 内部接线）。
                            vk_frequency_enabled: app_settings.vk_frequency_enabled,
                            ..kynoptic_core::collector::CollectorSettings::default()
                        };
                        let db_str = owner_db.to_string_lossy().into_owned();
                        // 审查 P1：registry 过滤后空集 = 启动失败，不得照常
                        // 启动 0 监控器空采（图标绿色、writer 永不落库、
                        // 30 分钟后 stalled 被看门狗 kill 复活成循环）。视为
                        // 启动失败留档 + 置 COLLECTOR_FAILED，且不进入 60s
                        // 空转重试（避免无限刷留档文件）；仅当设置再次变更
                        // （新的 Start 命令）时才重新评估。
                        if enabled.is_empty() {
                            let raw = kynoptic_dash::settings::load(&owner_db)
                                .enabled_monitors
                                .join(", ");
                            let msg = format!(
                                "[{}] 采集器启动失败: 启用监控器集合为空（settings 原始列表: [{}]；全部被 registry 过滤掉，可能是版本降级/设置损坏）\n（修正后保存设置即可恢复）\n",
                                chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
                                raw
                            );
                            let err_path = owner_db
                                .parent()
                                .unwrap_or(&owner_db)
                                .join("collector-error.log");
                            let _ = std::fs::write(&err_path, &msg);
                            log::error!("启用监控器集合为空，拒绝空采；已写入 {}", err_path.display());
                            COLLECTOR_FAILED.store(true, std::sync::atomic::Ordering::Relaxed);
                            COLLECTOR_RUNNING.store(false, std::sync::atomic::Ordering::Relaxed);
                            empty_set = true;
                            continue;
                        }
                        empty_set = false;
                        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            kynoptic_core::collector::start_collection_custom(&enabled, cs, &db_str)
                        })) {
                            Ok(c) => {
                                log::info!("采集器已启动({} 个监控器)", enabled.len());
                                collector = Some(c);
                                // 审查 P1：成功启动 = 心跳 stalled 判定的前提成立
                                COLLECTOR_RUNNING.store(true, std::sync::atomic::Ordering::Relaxed);
                                // 成功清零故障旗标（托盘 UI 从 Error 回 Running）
                                COLLECTOR_FAILED.store(false, std::sync::atomic::Ordering::Relaxed);
                                // 恢复成功：清除持久化错误留档
                                let _ = std::fs::remove_file(
                                    owner_db
                                        .parent()
                                        .unwrap_or(&owner_db)
                                        .join("collector-error.log"),
                                );
                            }
                            Err(_) => {
                                // 审查 P0：启动失败此前只在 stdout 喊一嗓子（托盘
                                // 无 console 等于没人看见），托盘照常活着、数据
                                // 静默归零——正是"库损坏=无声空采"的用户可见形态。
                                // 改为：持久化留档 + 置 COLLECTOR_FAILED + 每次刷新都重试。
                                let msg = kynoptic_core::db::diagnose_open_failure(
                                    std::path::Path::new(&db_str),
                                );
                                let err_path = owner_db
                                    .parent()
                                    .unwrap_or(&owner_db)
                                    .join("collector-error.log");
                                let _ = std::fs::write(
                                    &err_path,
                                    format!(
                                        "[{}] 采集器启动失败: {}
（每 60s 自动重试；dashboard 设置页可见此文件名）
",
                                        chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
                                        msg
                                    ),
                                );
                                log::error!("采集器启动失败: {msg}（已写 collector-error.log，60s 后重试）");
                                eprintln!(
                                    "采集器启动失败: {msg}，已写入 {}，60s 后重试",
                                    err_path.display()
                                );
                                COLLECTOR_FAILED.store(true, std::sync::atomic::Ordering::Relaxed);
                                // 审查 P1：未在跑就不得让心跳判定 stalled 依据成立
                                COLLECTOR_RUNNING
                                    .store(false, std::sync::atomic::Ordering::Relaxed);
                                // 保持 None:下一轮再试
                                // 约束：Err 分支必须把 collector 置 None。上面
                                // Resume/Start 前已对旧实例 shutdown()（505-507），
                                // 若这里仍留着 Some(已关停实例)，60s 重试条件
                                // collector.is_none()（487）永不成立——采集器静默
                                // 死亡且无法自愈；后续 Start/Pause 还会对同一实例
                                // 二次 shutdown()。take 掉即恢复重试条件。
                                let _ = collector.take();
                            }
                        }
                    }
                    CollectorCmd::Pause => {
                        paused = true;
                        if let Some(c) = collector.as_mut() {
                            // 置停机旗标 -> hook stop -> join writer -> 关 session
                            c.shutdown();
                        }
                        let _ = collector.take();
                        // 审查 P1：Pause 后无采集器,心跳回到纯时间戳语义
                        COLLECTOR_RUNNING.store(false, std::sync::atomic::Ordering::Relaxed);
                    }
                    CollectorCmd::Quit => {
                        if let Some(c) = collector.as_mut() {
                            c.shutdown();
                        }
                        COLLECTOR_RUNNING.store(false, std::sync::atomic::Ordering::Relaxed);
                        break;
                    }
                }
            }
        });
    // spawn 失败降级（审查 P1）：属主线程死 = 采集永久停止，panic 又会让
    // exit 旗标写不出去触发看门狗崩溃-重启循环；改为留痕 + Error 图标，
    // 进程继续活着让用户可见异常。
    if let Err(e) = &owner {
        log::error!("采集器属主线程启动失败（采集不可用）: {e}");
        COLLECTOR_FAILED.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    // 设置变更监听：dashboard 保存设置 -> SETTINGS_EPOCH +1 -> 自动重启采集器，
    // 并把 autostart 同步到注册表 Run 项（保存即生效，无需手动重启进程）。
    //
    // P1 防抖修复（实测：30 次保存后 +19 线程 +300 句柄不回落）：旧实现每次
    // 检测到变更立即 Start——设置页批量保存（拖动滑杆/逐项勾选）会 1 秒内连发
    // 多次重启，每次 Start 泄漏未 join 的 monitor 线程与句柄。现在变更后须
    // mtime/epoch 稳定 SETTING_DEBOUNCE_MS（2 秒）才触发一次重启，连续变更
    // 合并为一次生效。
    let watch_db = parsed.db.clone();
    let watch_tx = cmd_tx.clone();
    let mut last_epoch = kynoptic_dash::settings_epoch();
    // mtime 辅助触发（审查 P2：SETTINGS_EPOCH 是进程内原子量，跨进程写者
    // 触发不了；顺带覆盖"保存成功但进程在 epoch+1 前崩溃"的极窄丢失窗口）
    let watch_db_mtime = watch_db.clone();
    let settings_mtime = move || {
        std::fs::metadata(watch_db_mtime.with_file_name("settings.json"))
            .and_then(|m| m.modified())
            .ok()
    };
    let mut last_mtime = settings_mtime();
    let mut debounce = Debounce::new();
    // autostart 上次已同步值（P2：仅值实际变化才写注册表 Run 键）
    let mut last_applied_autostart: Option<bool> = None;
    let sw_handle = thread::Builder::new()
        .name("SettingsWatch".into())
        .spawn(move || loop {
            thread::sleep(std::time::Duration::from_millis(250));
            let m = settings_mtime();
            let e = kynoptic_dash::settings_epoch();
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            if e != last_epoch || m != last_mtime {
                last_epoch = e;
                last_mtime = m;
                // 变更（重）开防抖窗：连续保存不断顺延,稳定 2s 后才真正重启
                debounce.on_change(now_ms);
                continue;
            }
            if debounce.poll(now_ms) {
                let st = kynoptic_dash::settings::load(&watch_db);
                apply_autostart_if_changed(st.autostart, &mut last_applied_autostart);
                log::info!("设置已变更（防抖合并后生效），自动重启采集器");
                let _ = watch_tx.send(CollectorCmd::Start);
            }
        });
    // spawn 失败降级（审查 P1）：设置热重载失效只留痕，不许 panic 托盘
    if let Err(e) = &sw_handle {
        log::error!("设置监听线程启动失败（设置变更需手动重启托盘生效）: {e}");
    }

    // P1：settings.json 丢失不能静默。旧路径下文件消失（误删/重装残留清理）
    // 时 load() 无声回退 14 监控器缺省，用户"全开配置"凭空丢失且毫无痕迹。
    // 这里显式告警并立即用 Default 写出一份 settings.json：文件在场、可编辑、
    // 下次设置页打开不再是"隐形缺省"。写失败仅告警（不阻塞托盘启动）。
    // 文件存在但解析失败仍走 dash::load 内的 .corrupt.bak 留档逻辑，不在此处理。
    {
        let sp = kynoptic_dash::settings::settings_path(&parsed.db);
        if !sp.exists() {
            log::warn!(
                "settings.json 不存在({})，使用默认值；已尝试写出默认设置文件",
                sp.display()
            );
            eprintln!(
                "kynoptic-tray: settings.json 不存在({})，使用默认值",
                sp.display()
            );
            if let Err(e) = kynoptic_dash::settings::save(
                &parsed.db,
                &kynoptic_dash::settings::AppSettings::default(),
            ) {
                log::warn!("写出默认 settings.json 失败: {e}");
                eprintln!("kynoptic-tray: 写出默认 settings.json 失败: {e}");
            }
        }
    }

    // 启动时按当前设置同步一次自启动。
    // 审查 P1：settings.json 与注册表 Run 键不一致时（典型：安装器刚写好
    // Run 键，磁盘残留的旧 settings.json 里 autostart=false），**注册表为准**
    // 回写设置——注册表是用户在安装向导/仪表盘里最近一次显式操作的产物，
    // 不能被启动时序静默撤销。
    {
        let mut st = kynoptic_dash::settings::load(&parsed.db);
        let reg = kynoptic_dash::settings::autostart_registry_enabled_pub();
        if reg && !st.autostart {
            st.autostart = true;
            // 回写失败不能零留痕：注册表与 settings.json 此后持续不一致，
            // 至少落 tray.log 告警（不阻塞托盘启动）。
            if let Err(e) = kynoptic_dash::settings::save(&parsed.db, &st) {
                log::warn!("注册表→settings.json autostart 回写失败: {e}");
            }
        }
        apply_autostart(st.autostart);
    }

    // 初始启动采集
    let _ = cmd_tx.send(CollectorCmd::Start);

    // 托盘消息循环(阻塞直到 Quit;内部初始化失败会返回 false)
    let tray_ok = tray::run(parsed, cmd_tx.clone());
    if !tray_ok {
        let _ = cmd_tx.send(CollectorCmd::Quit);
    }

    // 优雅收尾:等属主线程完成置旗标 + join writer。
    // dashboard 服务线程不 join(listener 无关闭语义),随进程退出而终止。
    if let Ok(h) = owner {
        let _ = h.join();
    }

    // 优雅退出写旗标:watchdog 据此区分"用户主动退出"(不拉起)与"被杀/崩溃"(拉起)。
    // 被杀路径走不到这里,旗标不存在,watchdog 会重新拉起托盘。
    // 审查 P1：托盘 UI 初始化失败（RegisterClassW/CreateWindowExW/图标绘制/
    // NIM_ADD 耗尽）属基础设施故障，不得复用"用户主动退出"语义——否则
    // watchdog 永不拉起，采集随每次开机时序复现永久停止。此路径不写旗标：
    // watchdog 会按其坏托盘退避状态机（观察窗+连续失败退避）限速拉起。
    if tray_ok {
        // 写失败重试（审查 P2：旗标写丢会让 watchdog 把用户明确退出的托盘
        // 每分钟复活一次）；最终仍失败至少在心跳文件旁留痕。
        for _ in 0..3 {
            if std::fs::write(&exit_flag, chrono::Utc::now().to_rfc3339()).is_ok() {
                break;
            }
            thread::sleep(std::time::Duration::from_millis(300));
        }
    } else {
        log::error!("托盘 UI 初始化失败，退出且不写用户退出旗标（watchdog 将限速拉起）");
    }
}

/// 设置变更防抖窗：mtime/epoch 稳定该时长后才触发采集器重启
const SETTING_DEBOUNCE_MS: u64 = 2000;

/// 设置变更防抖（纯逻辑,单测覆盖）。连续变更不断重开窗口,稳定
/// SETTING_DEBOUNCE_MS 后 poll 返回 true 一次（触发一次重启）。
struct Debounce {
    last_change_ms: Option<u64>,
}

impl Debounce {
    fn new() -> Self {
        Self {
            last_change_ms: None,
        }
    }

    /// 检测到一次变更（重）开防抖窗。
    fn on_change(&mut self, now_ms: u64) {
        self.last_change_ms = Some(now_ms);
    }

    /// 无新变更时轮询：稳定超过防抖窗则触发一次并复位。返回是否应重启。
    fn poll(&mut self, now_ms: u64) -> bool {
        match self.last_change_ms {
            Some(t) if now_ms.saturating_sub(t) >= SETTING_DEBOUNCE_MS => {
                self.last_change_ms = None;
                true
            }
            _ => false,
        }
    }
}

/// autostart 是否需要写注册表（纯函数,单测覆盖）：上次已同步同一值则跳过。
fn autostart_needs_write(enable: bool, last_applied: &Option<bool>) -> bool {
    *last_applied != Some(enable)
}

/// autostart 仅在实际值变化时同步注册表 Run 键（P2：设置页每次保存都无谓
/// 写注册表 + 刷 daily_agg 的风暴路径之一）。返回是否执行了写。
fn apply_autostart_if_changed(enable: bool, last_applied: &mut Option<bool>) -> bool {
    if !autostart_needs_write(enable, last_applied) {
        return false;
    }
    apply_autostart(enable);
    *last_applied = Some(enable);
    true
}

/// 把 autostart 设置同步到注册表 Run 项（与 `kynoptic-ctl autostart` 同一键值）。
fn apply_autostart(enable: bool) {
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;
    const RUN_KEY_PATH: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
    const VALUE_NAME: &str = "Kynoptic";
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let Ok(key) = hkcu.open_subkey_with_flags(RUN_KEY_PATH, winreg::enums::KEY_SET_VALUE) else {
        eprintln!("autostart: 打开 Run 键失败");
        return;
    };
    if enable {
        match std::env::current_exe() {
            Ok(exe) => {
                let path = format!("\"{}\" --minimized", exe.display());
                if let Err(e) = key.set_value(VALUE_NAME, &path) {
                    eprintln!("autostart: 写入失败 {e}");
                }
            }
            Err(e) => eprintln!("autostart: 取 exe 路径失败 {e}"),
        }
    } else {
        let _ = key.delete_value(VALUE_NAME);
    }
    // Wave20 P0：autostart 双写源统一——看门狗计划任务随开关一起
    // ENABLE/DISABLE，否则设置页关了 autostart 后计划任务仍每分钟把
    // 被杀的托盘复活（"关了还弹回来"）。
    // 审查修复：schtasks 退出码此前被 `let _` 吞掉——任务被组策略禁用/
    // 删除时开关静默失效且零日志。失败必须留痕（log 落 tray.log）。
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let flag = if enable { "/ENABLE" } else { "/DISABLE" };
        match std::process::Command::new("schtasks")
            .args(["/Change", "/TN", "Kynoptic Watchdog", flag])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
        {
            Ok(out) if out.status.success() => {}
            Ok(out) => {
                let stderr = String::from_utf8_lossy(&out.stderr);
                log::warn!(
                    "schtasks /Change {} Kynoptic Watchdog 失败(退出码 {:?}) {}。看门狗计划任务的启停可能未生效（任务被禁用/删除/组策略拦截）",
                    flag,
                    out.status.code(),
                    stderr.trim()
                );
            }
            Err(e) => {
                log::warn!("schtasks 执行失败: {e}。看门狗计划任务的启停可能未生效");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // === 设置变更防抖（P1：30 次保存 +19 线程风暴） ===

    #[test]
    fn debounce_merges_burst_into_single_trigger() {
        // 模拟批量保存：0ms/300/600/900/1200 连续 5 次变更（旧实现 = 5 次
        // 采集器重启 = 5 份泄漏的 monitor 线程），稳定后只应触发 1 次。
        let mut d = Debounce::new();
        let mut triggers = 0;
        for t in [0u64, 300, 600, 900, 1200] {
            d.on_change(t);
        }
        // 窗口内轮询：不触发
        for t in [1500u64, 3000, 3100] {
            assert!(!d.poll(t), "稳定未满 2s 不应触发");
        }
        if d.poll(3200) {
            triggers += 1;
        }
        // 复位后不再重复触发
        assert!(!d.poll(3300));
        assert_eq!(triggers, 1, "连续 5 次保存必须合并为 1 次重启");
    }

    #[test]
    fn debounce_quiet_period_triggers_once_after_two_seconds() {
        let mut d = Debounce::new();
        d.on_change(10_000);
        assert!(!d.poll(10_000 + SETTING_DEBOUNCE_MS - 1));
        assert!(d.poll(10_000 + SETTING_DEBOUNCE_MS), "稳定 2s 即触发");
        assert!(!d.poll(10_000 + SETTING_DEBOUNCE_MS + 1), "触发一次即复位");
    }

    #[test]
    fn debounce_new_change_extends_window() {
        let mut d = Debounce::new();
        d.on_change(0);
        // 窗口将满时又来一次变更：重开窗口
        d.on_change(SETTING_DEBOUNCE_MS - 100);
        assert!(
            !d.poll(2 * SETTING_DEBOUNCE_MS - 200),
            "顺延后的窗口未满不触发"
        );
        assert!(d.poll(2 * SETTING_DEBOUNCE_MS - 100 + SETTING_DEBOUNCE_MS));
    }

    // === autostart 仅值变化时写注册表（P2） ===

    #[test]
    fn autostart_write_skipped_when_value_unchanged() {
        let mut last: Option<bool> = None;
        assert!(autostart_needs_write(true, &last), "首次必须同步");
        last = Some(true);
        assert!(!autostart_needs_write(true, &last), "值未变化跳过注册表写");
        assert!(autostart_needs_write(false, &last), "翻转必须写");
        assert!(!autostart_needs_write(false, &Some(false)));
    }

    #[test]
    fn apply_autostart_if_changed_noop_keeps_registry_untouched() {
        // 已同步 true 期间重复收到 true：不触碰注册表（无注册表副作用路径）
        let mut last: Option<bool> = Some(true);
        assert!(!apply_autostart_if_changed(true, &mut last));
        assert_eq!(last, Some(true));
    }
}
