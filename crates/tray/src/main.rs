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
mod icons;
mod state;
mod tray;

use std::sync::mpsc;
use std::thread;

use tray::CollectorCmd;

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let parsed = match args::parse(&argv) {
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
            if !h.is_null() && GetLastError() == ERROR_ALREADY_EXISTS {
                eprintln!("kynoptic-tray: 已有实例在运行,退出");
                return;
            }
        }
    }

    // 启动即清"用户主动退出"旗标:之后 watchdog 才有拉起依据
    let exit_flag = parsed
        .db
        .parent()
        .map(|p| p.join("tray-exit.flag"))
        .unwrap_or_else(|| std::path::PathBuf::from("tray-exit.flag"));
    let _ = std::fs::remove_file(&exit_flag);

    // dashboard 服务线程:与采集器同生命周期;只读打开,失败仅记录不阻塞托盘。
    // 健壮性:全新首装时本线程先于采集器跑,数据库文件还不存在,只读打开
    // 必失败且托盘无控制台（错误不可见,外面就是"拒绝连接"）。因此先等库
    // 文件就绪（至多 60s）,serve 失败再写日志文件,绝不静默消失。
    let dash_db = parsed.db.clone();
    let dash_port = parsed.port;
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

    // 采集器属主线程:Collector 只在本线程构造/持有/关停(所有权不跨线程)
    let (cmd_tx, cmd_rx) = mpsc::channel::<CollectorCmd>();
    let owner_db = parsed.db.clone();
    let owner_all = parsed.all;
    let owner = thread::Builder::new()
        .name("CollectorOwner".into())
        .spawn(move || {
            let mut collector: Option<kynoptic_core::collector::Collector> = None;
            while let Ok(cmd) = cmd_rx.recv() {
                match cmd {
                    CollectorCmd::Start => {
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
    thread::Builder::new()
        .name("SettingsWatch".into())
        .spawn(move || loop {
            thread::sleep(std::time::Duration::from_millis(1000));
            let e = kynoptic_dash::settings_epoch();
            if e != last_epoch {
                last_epoch = e;
                let st = kynoptic_dash::settings::load(&watch_db);
                apply_autostart(st.autostart);
                log::info!("设置已变更，自动重启采集器使其生效");
                let _ = watch_tx.send(CollectorCmd::Start);
            }
        })
        .expect("设置监听线程启动失败");

    // 启动时按当前设置同步一次自启动
    apply_autostart(kynoptic_dash::settings::load(&parsed.db).autostart);

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
