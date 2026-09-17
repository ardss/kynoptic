//! 托盘壳 CLI 参数:--db PATH / --port N / --all
//!
//! 纯逻辑,与 Win32 无关,单测覆盖。

use std::path::PathBuf;

use kynoptic_core::Error;

/// dashboard 默认端口(与 `kynoptic-ctl dashboard` 一致)
pub const DEFAULT_PORT: u16 = 8422;

const USAGE: &str = "usage: kynoptic-tray [--db PATH] [--port N] [--all] [--flag PATH]\n  --db PATH    database path (default: kynoptic_core::db::resolve_db_path)\n  --port N     dashboard TCP port on 127.0.0.1 (default 8422)\n  --all        enable the full monitor set instead of the default 14\n  --flag PATH  override tray-exit.flag path (default: exe dir; also KYNOPTIC_EXIT_FLAG env)\n";

/// 托盘壳启动配置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Args {
    pub db: PathBuf,
    pub port: u16,
    /// 透传采集器:启用全集监控器(registry::all_monitor_ids)
    pub all: bool,
    /// "用户主动退出"旗标路径覆盖(测试用;缺省走 exe 同目录,见 paths.rs)
    pub exit_flag: Option<PathBuf>,
}

/// 端口回退候选序列：preferred 起、逐个 +1，共 11 个（8422 被占时一路试到
/// 8432）。纯函数，单测覆盖。preferred 之后的 10 个端口即"面板静默死亡"
/// 的自救空间：回环上端口撞车（另一个服务占 8422）比换端口可接受得多。
pub fn candidate_ports(preferred: u16) -> Vec<u16> {
    (0..=10u16)
        .filter_map(|i| preferred.checked_add(i))
        .collect()
}

/// 在候选端口里挑第一个能 bind 127.0.0.1 的（探测 listener 立即 drop，
/// 正式 bind 由 kynoptic_dash::serve 完成——存在极窄 TOCTOU 窗口，可接受：
/// 单实例互斥体已保证没有第二个 kynoptic 抢同段端口）。
/// 全部失败返回 None（调用方回退 preferred，由 serve 报原始错误）。
pub fn pick_free_port(preferred: u16) -> Option<u16> {
    candidate_ports(preferred)
        .into_iter()
        .find(|&p| std::net::TcpListener::bind(("127.0.0.1", p)).is_ok())
}

/// 解析托盘壳参数。db 缺省走 core 统一解析;未知选项报错。
pub fn parse(args: &[String]) -> Result<Args, Error> {
    let mut port = DEFAULT_PORT;
    let mut db: Option<PathBuf> = None;
    let mut all = false;
    let mut exit_flag: Option<PathBuf> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--db" => {
                i += 1;
                let raw = args.get(i).cloned().unwrap_or_default();
                if raw.is_empty() {
                    return Err(Error::InvalidData("--db 需要路径".into()));
                }
                db = Some(PathBuf::from(raw));
            }
            "--port" => {
                i += 1;
                let raw = args.get(i).cloned().unwrap_or_default();
                port = raw
                    .parse::<u16>()
                    .map_err(|_| Error::InvalidData(format!("端口非法: {raw}(0-65535)")))?;
            }
            "--all" => all = true,
            // 退出旗标路径覆盖(与 watchdog 契约测试用;缺省 exe 同目录)
            "--flag" => {
                i += 1;
                let raw = args.get(i).cloned().unwrap_or_default();
                if raw.is_empty() {
                    return Err(Error::InvalidData("--flag 需要路径".into()));
                }
                exit_flag = Some(PathBuf::from(raw));
            }
            // 开机自启动/看门狗拉起时带的参数:托盘本来就无可视窗口,
            // 此处接受并忽略(历史上未实现,导致自启动启动即报错退出)。
            "--minimized" => {}
            other => return Err(Error::InvalidData(format!("未知选项: {other}\n\n{USAGE}"))),
        }
        i += 1;
    }
    Ok(Args {
        db: db.unwrap_or_else(kynoptic_core::db::resolve_db_path),
        port,
        all,
        exit_flag,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sv(args: &[&str]) -> Result<Args, Error> {
        parse(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn port_fallback_candidates_are_contiguous_eleven() {
        let v = candidate_ports(8422);
        assert_eq!(v.len(), 11);
        assert_eq!(v[0], 8422);
        assert_eq!(v[10], 8432);
        assert_eq!(
            v,
            (8422..=8432).collect::<Vec<u16>>(),
            "必须从 preferred 起逐个 +1"
        );
    }

    #[test]
    fn port_fallback_never_wraps_on_overflow() {
        // preferred 贴近 u16::MAX 时不得回绕到 0（回环低端口不允许偷偷占）
        let v = candidate_ports(u16::MAX);
        assert_eq!(v, vec![u16::MAX]);
    }

    #[test]
    fn pick_free_port_skips_occupied_and_reports_actual() {
        // 占住 8422，pick 应跳到下一个端口；选出的端口可被再次 bind 前提是
        // 先 drop 探测——这里只验证返回值 != 被占端口且落在候选集内
        let blocker = std::net::TcpListener::bind(("127.0.0.1", 8432)).unwrap();
        let picked = pick_free_port(8432).unwrap();
        assert_ne!(picked, 8432, "被占端口必须跳过");
        assert!(candidate_ports(8432).contains(&picked));
        drop(blocker);
    }

    #[test]
    fn port_zero_random_passes_through_candidates() {
        // port 0（随机空闲端口）路径不走回退，但 candidate_ports(0) 不panic
        assert_eq!(candidate_ports(0)[0], 0);
    }

    #[test]
    fn defaults_use_core_db_resolution_and_port() {
        let a = sv(&[]).unwrap();
        assert_eq!(a.port, DEFAULT_PORT);
        assert_eq!(a.db, kynoptic_core::db::resolve_db_path());
        assert!(!a.all);
    }

    #[test]
    fn explicit_db_port_all() {
        let a = sv(&["--db", "x/y.db", "--port", "9000", "--all"]).unwrap();
        assert_eq!(a.db, PathBuf::from("x/y.db"));
        assert_eq!(a.port, 9000);
        assert!(a.all);
    }

    #[test]
    fn port_zero_allowed() {
        assert_eq!(sv(&["--port", "0"]).unwrap().port, 0);
    }

    #[test]
    fn rejects_bad_port_and_unknown_flag() {
        assert!(sv(&["--port", "99999"]).is_err());
        assert!(sv(&["--port", "abc"]).is_err());
        assert!(sv(&["--gpu"]).is_err());
        assert!(sv(&["--db"]).is_err());
    }

    #[test]
    fn accepts_minimized_flag_written_by_autostart_and_installer() {
        // 回归:autostart 注册表与 watchdog 拉起都带 --minimized,
        // 历史上未实现该参数导致自启动启动即报错退出。
        assert!(sv(&["--minimized"]).is_ok());
        assert!(sv(&["--minimized", "--all"]).is_ok());
    }

    #[test]
    fn flag_path_override() {
        // 回归:--flag 覆盖退出旗标路径(测试注入用);缺省为 None -> exe 同目录
        let a = sv(&[]).unwrap();
        assert_eq!(a.exit_flag, None);
        let a = sv(&["--flag", r"C:\tmp\flag"]).unwrap();
        assert_eq!(a.exit_flag, Some(PathBuf::from(r"C:\tmp\flag")));
        assert!(sv(&["--flag"]).is_err());
        assert!(sv(&["--flag", ""]).is_err());
    }
}
