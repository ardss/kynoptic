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

/// 采集停滞阈值（审查 P1）：flush 距今超过该秒数且采集器在跑,心跳打
/// stalled:true。30s 写一轮心跳、正常批次间隔远小于此,300s ≈ 连续 10 个
/// 心跳周期无落库,足以区分"空闲无输入"与"writer 挂死"。
const HEARTBEAT_STALLED_SECS: i64 = 300;

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut parsed = match args::parse(&argv) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("kynoptic-tray: {e}");
            std::process::exit(2);
        }
    };

    // 单实例互斥体:防双开,同时是 watchdog 的存活探针
    {
        use windows_sys::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
        use windows_sys::Win32::System::Threading::CreateMutexW;
        let name: Vec<u16> = format!("{}\0", paths::SINGLE_INSTANCE_MUTEX_NAME)
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
                eprintln!("kynoptic-tray: 已有实例在运行,退出");
                return;
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
                let _ = port_tx.send(Some(cand));
                match kynoptic_dash::serve(&dash_db, cand, true) {
                    Ok(()) => return, // 正常退出路径（进程结束）
                    Err(e) => {
                        last_err = Some(format!("serve 127.0.0.1:{cand}: {e}"));
                        continue;
                    }
                }
            }
            // 全部候选失败:port.txt 写 "unavailable",错误留档 dashboard-error.log
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
        })
        .expect("dashboard 线程启动失败");

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
    // 每次成功落库刷新);采集运行中且 flush 停滞超 HEARTBEAT_STALLED_SECS(300s)
    // 时打 stalled:true,watchdog 侧 classify_heartbeat 据此判为过期并 kill。
    // 旧版纯时间戳内容仍被 watchdog 兼容解析(向后兼容,滚动升级期两代共存)。
    {
        let hb = paths::resolve_heartbeat();
        thread::Builder::new()
            .name("Heartbeat".into())
            .spawn(move || loop {
                let now = chrono::Utc::now();
                let flush = kynoptic_core::collector::last_flush_epoch();
                let running = COLLECTOR_RUNNING.load(std::sync::atomic::Ordering::Relaxed);
                // flush==0 = 本进程尚未落过库(启动初期正常),不误报;真正
                // 挂死场景是 flush 曾前进后停滞,由 300s 阈值覆盖。
                let stalled =
                    running && flush > 0 && now.timestamp() - flush as i64 > HEARTBEAT_STALLED_SECS;
                let content = format!(
                    "{{\"pid\":{},\"ts\":\"{}\",\"flush\":{},\"stalled\":{}}}",
                    std::process::id(),
                    now.to_rfc3339(),
                    flush,
                    stalled
                );
                let _ = std::fs::write(&hb, content);
                thread::sleep(std::time::Duration::from_secs(30));
            })
            .expect("心跳线程启动失败");
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
            // 审查 P0：启动失败后不能只等下一条命令才重试——改为 60s 超时
            // 醒来一次，采集器缺位且未暂停时自动重试（库被占/磁盘满恢复后
            // 自愈，无需用户干预）。超时重试走 Resume 语义：collector 为 None
            // 时 Resume 分支的"清掉上一实例"自然跳过。
            loop {
                let cmd = match cmd_rx.recv_timeout(std::time::Duration::from_secs(60)) {
                    Ok(c) => c,
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        if collector.is_none() && !paused {
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
                        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            kynoptic_core::collector::start_collection_custom(&enabled, cs, &db_str)
                        })) {
                            Ok(c) => {
                                log::info!("采集器已启动({} 个监控器)", enabled.len());
                                collector = Some(c);
                                // 审查 P1：成功启动 = 心跳 stalled 判定的前提成立
                                COLLECTOR_RUNNING.store(true, std::sync::atomic::Ordering::Relaxed);
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
                                // 改为：持久化留档 + 每次刷新都重试。
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
                                eprintln!(
                                    "采集器启动失败: {msg}，已写入 {}，60s 后重试",
                                    err_path.display()
                                );
                                // 审查 P1：未在跑就不得让心跳判定 stalled 依据成立
                                COLLECTOR_RUNNING
                                    .store(false, std::sync::atomic::Ordering::Relaxed);
                                // 保持 None:下一轮再试
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
        })
        .expect("采集器属主线程启动失败");

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
    thread::Builder::new()
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
        })
        .expect("设置监听线程启动失败");

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
            let _ = kynoptic_dash::settings::save(&parsed.db, &st);
        }
        apply_autostart(st.autostart);
    }

    // 初始启动采集
    let _ = cmd_tx.send(CollectorCmd::Start);

    // 托盘消息循环(阻塞直到 Quit;内部 NIM_ADD 失败会返回 false)
    if !tray::run(parsed, cmd_tx.clone()) {
        let _ = cmd_tx.send(CollectorCmd::Quit);
    }

    // 优雅收尾:等属主线程完成置旗标 + join writer。
    // dashboard 服务线程不 join(listener 无关闭语义),随进程退出而终止。
    let _ = owner.join();

    // 优雅退出写旗标:watchdog 据此区分"用户主动退出"(不拉起)与"被杀/崩溃"(拉起)。
    // 被杀路径走不到这里,旗标不存在,watchdog 会重新拉起托盘。
    let _ = std::fs::write(&exit_flag, chrono::Utc::now().to_rfc3339());
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
