//! 应用分类配置
//!
//! 从 config/app_categories.json 加载分类规则，编译时嵌入二进制

use serde::Deserialize;
use std::collections::HashMap;
use std::sync::OnceLock;

#[derive(Deserialize)]
struct ConfigFile {
    categories: HashMap<String, Vec<String>>,
    diary_activities: HashMap<String, String>,
}

struct AppCategoryConfig {
    entries: Vec<(Vec<String>, &'static str)>,
    diary: HashMap<String, String>,
}

static CONFIG: OnceLock<AppCategoryConfig> = OnceLock::new();

fn load() -> &'static AppCategoryConfig {
    CONFIG.get_or_init(|| {
        let raw: ConfigFile = serde_json::from_str(include_str!("../config/app_categories.json"))
            .unwrap_or_else(|e| {
                log::error!("加载 app_categories.json 失败: {e}");
                ConfigFile {
                    categories: HashMap::new(),
                    diary_activities: HashMap::new(),
                }
            });

        let order = [
            ("game", "game"),
            ("development", "development"),
            ("browser", "browser"),
            ("communication", "communication"),
            ("media", "media"),
            ("design", "design"),
            ("writing", "writing"),
            ("terminal", "terminal"),
        ];

        let entries: Vec<(Vec<String>, &'static str)> = order
            .iter()
            .filter_map(|(key, label)| {
                raw.categories
                    .get(*key)
                    .map(|patterns| (patterns.clone(), *label))
            })
            .collect();

        AppCategoryConfig {
            entries,
            diary: raw.diary_activities,
        }
    })
}

/// 对应用名进行分类
pub fn classify_app(app: &str) -> &'static str {
    let lower = app.to_lowercase();
    let lower = lower.trim_end_matches(".exe");
    let cfg = load();
    for (patterns, label) in &cfg.entries {
        for p in patterns {
            if lower.contains(p.as_str()) {
                return label;
            }
        }
    }
    "other"
}

/// 获取日记中应用对应的中文活动描述
pub fn diary_activity(app: &str) -> Option<&str> {
    let lower = app.to_lowercase();
    let lower = lower.trim_end_matches(".exe");
    let cfg = load();
    cfg.diary.get(lower).map(|s| s.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_known_categories() {
        assert_eq!(classify_app("steam.exe"), "game");
        assert_eq!(classify_app("Code.exe"), "development");
        assert_eq!(classify_app("chrome.exe"), "browser");
        assert_eq!(classify_app("WeChat.exe"), "communication");
        assert_eq!(classify_app("vlc.exe"), "media");
        assert_eq!(classify_app("photoshop.exe"), "design");
        assert_eq!(classify_app("WINWORD.EXE"), "writing");
        // 注意：windowsterminal 同时出现在 development 和 terminal 列表，
        // order 中 development 优先，故归为 development（开发者终端场景）。
        // terminal 类别实际只覆盖 bash/wsl 等未与 development 重叠的项。
        assert_eq!(classify_app("bash.exe"), "terminal");
        assert_eq!(classify_app("wsl.exe"), "terminal");
    }

    #[test]
    fn classify_unknown_is_other() {
        assert_eq!(classify_app("randomapp.exe"), "other");
        assert_eq!(classify_app(""), "other");
    }

    #[test]
    fn classify_case_insensitive_and_strips_exe() {
        // 大小写不敏感
        assert_eq!(classify_app("STEAM"), "game");
        assert_eq!(classify_app("Steam.exe"), "game");
        assert_eq!(classify_app("steam"), "game");
    }

    #[test]
    fn classify_precedence_game_over_terminal() {
        // order 数组中 game 在 terminal 之前，故 javaw 应归为 game
        // （javaw 出现在 game 列表，不在 terminal 列表，验证不误归类即可）
        assert_eq!(classify_app("javaw.exe"), "game");
    }

    #[test]
    fn classify_substring_match() {
        // 分类基于 contains 子串匹配
        assert_eq!(classify_app("my-custom-chrome-wrapper.exe"), "browser");
    }

    #[test]
    fn diary_activity_known() {
        assert_eq!(diary_activity("code.exe"), Some("写代码"));
        assert_eq!(diary_activity("Chrome"), Some("浏览网页"));
        assert_eq!(diary_activity("wechat"), Some("聊天"));
    }

    #[test]
    fn diary_activity_unknown_is_none() {
        assert_eq!(diary_activity("steam.exe"), None);
        assert_eq!(diary_activity(""), None);
    }
}
