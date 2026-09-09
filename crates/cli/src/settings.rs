//! 应用设置：`settings.json`（与 kynoptic.db 同目录）。
//!
//! 只管设置读写，绝不触碰 events 表。采集器未来按 `enabled_monitors`
//! 启动；dashboard 端点经本模块读写同一份文件（单一事实源）。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use kynoptic_core::registry;

/// dashboard 默认端口（与 dashboard.rs DEFAULT_PORT 一致）。
pub const DEFAULT_DASHBOARD_PORT: u16 = 8422;

/// 应用设置文件内容。缺省值见 [`AppSettings::default`]。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppSettings {
    /// 启用的监控器 id 集合（缺省 = registry::default_enabled_ids()）。
    #[serde(default = "default_enabled_monitors")]
    pub enabled_monitors: Vec<String>,
    /// 开机自启动（缺省读现有注册表 Run 项；无则 false）。
    #[serde(default)]
    pub autostart: bool,
    /// dashboard 监听端口（仅记录，dashboard 启动参数仍可覆盖）。
    #[serde(default = "default_dashboard_port")]
    pub dashboard_port: u16,
}

fn default_enabled_monitors() -> Vec<String> {
    registry::default_enabled_ids()
        .into_iter()
        .map(String::from)
        .collect()
}

fn default_dashboard_port() -> u16 {
    DEFAULT_DASHBOARD_PORT
}

impl Default for AppSettings {
    fn default() -> Self {
        AppSettings {
            enabled_monitors: default_enabled_monitors(),
            autostart: false,
            dashboard_port: DEFAULT_DASHBOARD_PORT,
        }
    }
}

/// settings.json 路径：与 db 同目录（resolve_db_path() 旁）。
pub fn settings_path(db_path: &Path) -> PathBuf {
    match db_path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir.join("settings.json"),
        _ => PathBuf::from("settings.json"),
    }
}

/// 读取设置；文件不存在时返回缺省（autostart 先探测现有注册表 Run 项，
/// 与 `kynoptic-ctl autostart status` 同一事实源）。
pub fn load(db_path: &Path) -> AppSettings {
    let path = settings_path(db_path);
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return AppSettings {
            autostart: crate::autostart::is_enabled().unwrap_or(false),
            ..AppSettings::default()
        };
    };
    match serde_json::from_str::<AppSettings>(&raw) {
        Ok(s) => s,
        Err(_) => AppSettings {
            autostart: crate::autostart::is_enabled().unwrap_or(false),
            ..AppSettings::default()
        },
    }
}

/// 写盘（原子性：先写 .tmp 再改名，避免半截 JSON）。
pub fn save(db_path: &Path, settings: &AppSettings) -> std::io::Result<()> {
    let path = settings_path(db_path);
    let tmp = path.with_extension("json.tmp");
    let json = serde_json::to_string_pretty(settings).map_err(std::io::Error::other)?;
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, &path)
}

/// 校验 id 集合：全部必须在 MONITOR_REGISTRY 中。返回第一个非法 id。
pub fn first_invalid_id(ids: &[String]) -> Option<String> {
    ids.iter()
        .find(|id| !registry::all_monitor_ids().contains(&id.as_str()))
        .cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "kyn-settings-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn defaults_match_registry() {
        let s = AppSettings::default();
        let defaults: Vec<String> = registry::default_enabled_ids()
            .into_iter()
            .map(String::from)
            .collect();
        assert_eq!(s.enabled_monitors, defaults);
        assert_eq!(s.enabled_monitors.len(), 14);
        assert!(!s.autostart);
        assert_eq!(s.dashboard_port, DEFAULT_DASHBOARD_PORT);
    }

    #[test]
    fn settings_path_is_db_sibling() {
        assert_eq!(
            settings_path(Path::new("C:/data/kynoptic.db")),
            PathBuf::from("C:/data/settings.json")
        );
        assert_eq!(
            settings_path(Path::new("kynoptic.db")),
            PathBuf::from("settings.json")
        );
    }

    #[test]
    fn save_then_load_roundtrip() {
        let dir = tmpdir("roundtrip");
        let db = dir.join("kyn.db");
        let s = AppSettings {
            enabled_monitors: vec!["window".into(), "keyboard_hook".into()],
            autostart: true,
            dashboard_port: 9000,
        };
        save(&db, &s).unwrap();
        assert_eq!(load(&db), s);
        assert!(dir.join("settings.json").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn load_missing_file_returns_defaults() {
        let dir = tmpdir("missing");
        let db = dir.join("kyn.db");
        let s = load(&db);
        assert_eq!(s.enabled_monitors.len(), 14);
        assert!(!s.autostart, "测试环境无自启动注册表项");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn load_corrupt_file_falls_back_to_defaults() {
        let dir = tmpdir("corrupt");
        let db = dir.join("kyn.db");
        std::fs::write(settings_path(&db), "{ not json").unwrap();
        let s = load(&db);
        assert_eq!(s, AppSettings::default());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn partial_json_fills_defaults() {
        let dir = tmpdir("partial");
        let db = dir.join("kyn.db");
        std::fs::write(settings_path(&db), r#"{"autostart": true}"#).unwrap();
        let s = load(&db);
        assert!(s.autostart);
        assert_eq!(s.enabled_monitors.len(), 14);
        assert_eq!(s.dashboard_port, DEFAULT_DASHBOARD_PORT);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn id_validation_against_registry() {
        assert_eq!(first_invalid_id(&["window".into()]), None);
        assert_eq!(
            first_invalid_id(&["window".into(), "nope".into()]),
            Some("nope".into())
        );
        assert_eq!(first_invalid_id(&[]), None);
    }
}
