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
    /// Wave22：classify_app 删除后 entries 仅存档（旧配置文件兼容读取），
    /// 不再有消费方
    #[allow(dead_code)]
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

// Wave22：classify_app 已删除——零调用的第二套分类命名（与 settings
// categories 引擎并存必被误接）。唯一权威分类是 CategoryRule::matches。
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
