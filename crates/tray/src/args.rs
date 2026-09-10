//! 托盘壳 CLI 参数:--db PATH / --port N / --all
//!
//! 纯逻辑,与 Win32 无关,单测覆盖。

use std::path::PathBuf;

use kynoptic_core::Error;

/// dashboard 默认端口(与 `kynoptic-ctl dashboard` 一致)
pub const DEFAULT_PORT: u16 = 8422;

const USAGE: &str = "usage: kynoptic-tray [--db PATH] [--port N] [--all]\n  --db PATH  database path (default: kynoptic_core::db::resolve_db_path)\n  --port N   dashboard TCP port on 127.0.0.1 (default 8422)\n  --all      enable the full monitor set instead of the default 14\n";

/// 托盘壳启动配置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Args {
    pub db: PathBuf,
    pub port: u16,
    /// 透传采集器:启用全集监控器(registry::all_monitor_ids)
    pub all: bool,
}

/// 解析托盘壳参数。db 缺省走 core 统一解析;未知选项报错。
pub fn parse(args: &[String]) -> Result<Args, Error> {
    let mut port = DEFAULT_PORT;
    let mut db: Option<PathBuf> = None;
    let mut all = false;
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
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sv(args: &[&str]) -> Result<Args, Error> {
        parse(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>())
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
}
