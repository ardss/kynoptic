//! 应用设置：`settings.json`（与 kynoptic.db 同目录）。
//!
//! dashboard v2 起设置读写实现迁入 `kynoptic_dash::settings`（与 dashboard
//! 端点同源、单一事实源）；本模块仅保留 cli 侧的 `load`——文件缺失/损坏时
//! autostart 缺省探测注册表 Run 项（与 `kynoptic-ctl autostart status` 同源）。

use std::path::Path;

#[allow(unused_imports)]
// re-export 供 cli 内其余模块与测试复用（bin crate 无法 pub use 逃逸）
pub use kynoptic_dash::settings::{
    first_invalid_id, save, settings_path, AppSettings, DEFAULT_DASHBOARD_PORT,
};

/// 读取设置；文件不存在/损坏时 autostart 先探测现有注册表 Run 项，
/// 其余字段取缺省。绝不触碰 events 表。
pub fn load(db_path: &Path) -> AppSettings {
    match kynoptic_dash::settings::try_load(db_path) {
        Some(s) => s,
        None => AppSettings {
            autostart: crate::autostart::is_enabled().unwrap_or(false),
            ..AppSettings::default()
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

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
        let defaults: Vec<String> = kynoptic_core::registry::default_enabled_ids()
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
        let mut s = AppSettings {
            enabled_monitors: vec!["window".into(), "keyboard_hook".into()],
            autostart: true,
            dashboard_port: 9000,
            input_counts_only: true,
            ..AppSettings::default()
        };
        s.daily_goal_minutes = 480;
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
        assert_eq!(s.enabled_monitors.len(), 14);
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
