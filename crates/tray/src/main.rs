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
        let name: Vec<u16> = "Local\\KynopticTrayMutex\0".encode_utf16().collect();
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
        let _ = ghost::close_all_open_sessions(&parsed.db);
    }

    // dashboard 端口回退（P0：8422 被占 = 面板静默死亡）：在 spawn 服务线程
    // 前选好实际端口，托盘菜单 Open Dashboard 也用同一个端口，不会打开死链接。
    // port 0（随机空闲端口）语义保留，不走回退。
    let dash_port_requested = parsed.port;
    let dash_port = if dash_port_requested == 0 {
        0
    } else {
        args::pick_free_port(dash_port_requested).unwrap_or(dash_port_requested)
    };
    parsed.port = dash_port;

    // 采集心跳:每 30s touch exe 同目录心跳文件(RFC3339 时间戳)。
    // watchdog 除互斥体探活外还会检查心跳新鲜度——进程活着但采集主循环挂死
    // 时,心跳停止,watchdog 据此 kill 并重启(4-8 小时空洞的根因修复)。
    // 写失败静默忽略:心跳缺失只是退化为旧的探活行为,不影响采集本身。
    {
        let hb = paths::resolve_heartbeat();
        thread::Builder::new()
            .name("Heartbeat".into())
            .spawn(move || loop {
                let _ = std::fs::write(&hb, chrono::Utc::now().to_rfc3339());
                thread::sleep(std::time::Duration::from_secs(30));
            })
            .expect("心跳线程启动失败");
    }

    // dashboard 服务线程:与采集器同生命周期;只读打开,失败仅记录不阻塞托盘。
    // 健壮性:全新首装时本线程先于采集器跑,数据库文件还不存在,只读打开
    // 必失败且托盘无控制台（错误不可见,外面就是"拒绝连接"）。因此先等库
    // 文件就绪（至多 60s）,serve 失败再写日志文件,绝不静默消失。
    let dash_db = parsed.db.clone();
    let _dash_handle = thread::Builder::new()
        .name("Dashboard".into())
        .spawn(move || {
            for _ in 0..120 {
                if dash_db.exists() {
                    break;
                }
                thread::sleep(std::time::Duration::from_millis(500));
            }
            match kynoptic_dash::serve(&dash_db, dash_port, true) {
                Ok(()) => {}
                Err(e) => {
                    let msg = format!(
                        "[{}] dashboard 服务退出: {e}\n",
                        chrono::Utc::now().to_rfc3339()
                    );
                    eprint!("{msg}");
                    if let Some(dir) = dash_db.parent() {
                        let _ = std::fs::write(dir.join("dashboard-error.log"), &msg);
                    }
                }
            }
        })
        .expect("dashboard 线程启动失败");

    // 端口落盘 + 换端口提示（托盘壳刻意不弹气泡：tray.rs 铁律 NIF_INFO 永不
    // 使用，退化为 log + 文件）。dashboard-port.txt 始终写实际端口，供排障与
    // 外部工具读取；仅当相对请求端口发生变化时额外记一条显式告警。
    if dash_port_requested != 0 {
        if let Some(dir) = parsed.db.parent() {
            let _ = std::fs::write(dir.join("dashboard-port.txt"), format!("{dash_port}\n"));
        }
        if dash_port != dash_port_requested {
            let msg = format!(
                "面板已换端口：{dash_port_requested} 被占用，dashboard 改用 {dash_port}（http://127.0.0.1:{dash_port}，已写入 dashboard-port.txt）"
            );
            log::warn!("{msg}");
            eprintln!("kynoptic-tray: {msg}");
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
            while let Ok(cmd) = cmd_rx.recv() {
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
                            }
                            Err(_) => {
                                eprintln!("采集器启动失败(DB 不可写?),采集暂停");
                                // 保持 None:下次 Resume 再试
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
                    }
                    CollectorCmd::Quit => {
                        if let Some(c) = collector.as_mut() {
                            c.shutdown();
                        }
                        break;
                    }
                }
            }
        })
        .expect("采集器属主线程启动失败");

    // 设置变更监听：dashboard 保存设置 -> SETTINGS_EPOCH +1 -> 自动重启采集器，
    // 并把 autostart 同步到注册表 Run 项（保存即生效，无需手动重启进程）。
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
    thread::Builder::new()
        .name("SettingsWatch".into())
        .spawn(move || loop {
            thread::sleep(std::time::Duration::from_millis(1000));
            let m = settings_mtime();
            let e = kynoptic_dash::settings_epoch();
            let mtime_changed = m != last_mtime;
            if e != last_epoch || mtime_changed {
                last_epoch = e;
                last_mtime = m;
                let st = kynoptic_dash::settings::load(&watch_db);
                apply_autostart(st.autostart);
                log::info!("设置已变更，自动重启采集器使其生效");
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
