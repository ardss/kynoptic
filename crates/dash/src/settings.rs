//! 应用设置：`settings.json`（与 kynoptic.db 同目录）。
//!
//! 从 `crates/cli/src/settings.rs` 迁入（dashboard 服务迁入本 crate 后，
//! 设置读写与 dashboard 端点同源）。只管设置读写，绝不触碰 events 表。
//! 采集器按 `enabled_monitors` 启动；dashboard 端点经本模块读写同一份
//! 文件（单一事实源）。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use kynoptic_core::registry;

/// dashboard 默认端口。
pub const DEFAULT_DASHBOARD_PORT: u16 = 8422;

/// 窗口分类规则（正则，匹配 app_name 或 window_title，大小写不敏感）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CategoryRule {
    pub name: String,
    /// 不区分大小写的子串/正则模式（简化：按空格拆 token，任一 token 命中即归类）
    pub pattern: String,
}

/// 内置默认分类（可被 settings.json 覆盖）。匹配顺序即优先级，未命中 → 其他。
pub fn default_categories() -> Vec<CategoryRule> {
    vec![
        CategoryRule::rule("开发", "code dev vscode visualstudio git github powershell cmd terminal windowsterminal zcode idea pycharm jetbrains sublime vim emacs neovim clang rust cargo python node npm"),
        CategoryRule::rule("浏览", "chrome msedge edge firefox browser safari opera vivaldi brave"),
        CategoryRule::rule("通讯", "wechat weixin qq telegram discord slack dingtalk feishu outlook mail thunderbird"),
        CategoryRule::rule("娱乐", "steam bilibili youtube spotify music netease douyin tiktok game epic"),
        CategoryRule::rule("文档", "word excel powerpoint notepad pdf office wps typura obsidian notion"),
        CategoryRule::rule("设计", "photoshop figma blender gimp inkscape premiere davinci affinity canva"),
    ]
}

impl CategoryRule {
    pub(crate) fn rule(name: &str, pattern: &str) -> Self {
        Self { name: name.to_string(), pattern: pattern.to_string() }
    }
    /// app/title 是否命中该规则（token 子串匹配，大小写不敏感）。
    pub fn matches(&self, app: &str, title: &str) -> bool {
        let hay = format!("{} {}", app, title).to_lowercase();
        self.pattern.split_whitespace().any(|tok| hay.contains(tok))
    }
}

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
    /// 输入采集边界：true（默认）= 只存每分钟计数，不存按键内容（隐私红线）；
    /// false = 逐键明细（opt-in，知情用户显式开启）。
    #[serde(default = "default_input_counts_only")]
    pub input_counts_only: bool,
    /// 每日活跃目标（分钟，0 = 关闭目标进度条）。
    #[serde(default = "default_daily_goal_minutes")]
    pub daily_goal_minutes: u32,
    /// 窗口分类规则（正则 token，匹配顺序即优先级，未命中 → 其他）。
    #[serde(default = "default_categories")]
    pub categories: Vec<CategoryRule>,
}

fn default_daily_goal_minutes() -> u32 {
    480
}

fn default_input_counts_only() -> bool {
    true
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
            input_counts_only: true,
            daily_goal_minutes: 480,
            categories: default_categories(),
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

/// 读取设置；文件不存在或损坏时返回 None（调用方可补自己的
/// autostart 缺省探测——cli 版即用注册表 Run 项兜底）。
pub fn try_load(db_path: &Path) -> Option<AppSettings> {
    let raw = std::fs::read_to_string(settings_path(db_path)).ok()?;
    serde_json::from_str::<AppSettings>(&raw).ok()
}

/// 读取设置；文件不存在/损坏时返回缺省（autostart 先探测现有注册表
/// Run 项，与 `kynoptic-ctl autostart status` 同一事实源）。
pub fn load(db_path: &Path) -> AppSettings {
    try_load(db_path).unwrap_or_else(|| AppSettings {
        autostart: autostart_registry_enabled(),
        ..AppSettings::default()
    })
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

/// 注册表 Run 项里 Kynoptic 是否已启用（缺省 autostart 的探测源）。
/// 仅 Windows 有实际意义；其他平台恒 false。
#[cfg(target_os = "windows")]
fn autostart_registry_enabled() -> bool {
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;
    const RUN_KEY_PATH: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
    const VALUE_NAME: &str = "Kynoptic";
    RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey(RUN_KEY_PATH)
        .and_then(|k| k.get_value::<String, _>(VALUE_NAME))
        .map(|v| !v.is_empty())
        .unwrap_or(false)
}

#[cfg(not(target_os = "windows"))]
fn autostart_registry_enabled() -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "kyn-dash-settings-{tag}-{}-{}",
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
        assert!(s.input_counts_only);
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
            input_counts_only: true,
            daily_goal_minutes: 480,
            categories: default_categories(),
        };
        save(&db, &s).unwrap();
        assert_eq!(try_load(&db), Some(s));
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
        assert_eq!(try_load(&db), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn load_corrupt_file_falls_back_to_defaults() {
        let dir = tmpdir("corrupt");
        let db = dir.join("kyn.db");
        std::fs::write(settings_path(&db), "{ not json").unwrap();
        assert_eq!(try_load(&db), None);
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
