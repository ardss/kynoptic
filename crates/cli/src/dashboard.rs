//! `kynoptic-ctl dashboard` — CLI 参数解析入口（服务本体在 kynoptic-dash crate）。
//!
//! dashboard v2 起服务整体迁入 `kynoptic_dash::serve`（与 tray 共用唯一实现），
//! 本模块只保留 `--port` / `--db` 解析与 db 存在性检查。

use std::path::PathBuf;

use kynoptic_core::{Error, Result};

pub use kynoptic_dash::DEFAULT_PORT;

const USAGE_DASH: &str = "usage: kynoptic-ctl dashboard [--port N] [--db PATH]\n  --port N   TCP port on 127.0.0.1 (default 8422, 0 = random free port)\n  --db PATH  kynoptic.db path (default: KYNOPTIC_DB > exe-relative > cwd)\n";

/// 解析 `--port` / `--db`。端口非法或超出范围报错；db 缺省走 core 统一解析。
pub fn parse_args(args: &[String]) -> Result<(u16, PathBuf)> {
    let mut port = DEFAULT_PORT;
    let mut db: Option<PathBuf> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--port" => {
                i += 1;
                let raw = args.get(i).cloned().unwrap_or_default();
                port = raw
                    .parse::<u16>()
                    .map_err(|_| Error::InvalidData(format!("端口非法: {raw}（0-65535）")))?;
            }
            "--db" => {
                i += 1;
                let raw = args.get(i).cloned().unwrap_or_default();
                if raw.is_empty() {
                    return Err(Error::InvalidData("--db 需要路径".into()));
                }
                db = Some(PathBuf::from(raw));
            }
            other => {
                return Err(Error::InvalidData(format!(
                    "未知选项: {other}\n\n{USAGE_DASH}"
                )))
            }
        }
        i += 1;
    }
    Ok((port, db.unwrap_or_else(kynoptic_core::db::resolve_db_path)))
}

/// `kynoptic-ctl dashboard [--port N] [--db PATH]` 入口。
pub fn cmd_dashboard(args: &[String]) -> Result<()> {
    let (port, db_path) = parse_args(args)?;
    if !db_path.exists() {
        return Err(Error::InvalidData(format!(
            "数据库不存在: {}（先用采集器/ctl 生成，dashboard 不建库不迁移）",
            db_path.display()
        )));
    }
    kynoptic_dash::serve(&db_path, port, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dash_args_defaults_and_db_override() {
        let (port, db) = parse_args(&[]).unwrap();
        assert_eq!(port, DEFAULT_PORT);
        assert_eq!(db, kynoptic_core::db::resolve_db_path());
        let (port, db) =
            parse_args(&["--port".into(), "0".into(), "--db".into(), "x/y.db".into()]).unwrap();
        assert_eq!(port, 0);
        assert_eq!(db, PathBuf::from("x/y.db"));
    }

    #[test]
    fn dash_args_rejects_bad_port_and_flag() {
        assert!(parse_args(&["--port".into(), "99999".into()]).is_err());
        assert!(parse_args(&["--port".into(), "abc".into()]).is_err());
        assert!(parse_args(&["--gpu".into()]).is_err());
        assert!(parse_args(&["--db".into()]).is_err());
    }

    #[test]
    fn default_port_matches_settings() {
        assert_eq!(DEFAULT_PORT, 8422);
        assert_eq!(
            DEFAULT_PORT,
            kynoptic_dash::settings::DEFAULT_DASHBOARD_PORT
        );
    }
}
