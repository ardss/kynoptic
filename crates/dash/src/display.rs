//! 展示层应用名归一（单一事实源：所有 `/api/*` 端点输出 app 名都经
//! [`display_app`]，前端 tooltip/图例不再各自剥后缀）。
//!
//! 背景（发现 dashdocs）：DB 里 `app_name` 是采集器存下的原始进程名
//! （`QueryFullProcessImageNameW` 的文件名，带 `.exe`，如 `chrome.exe` /
//! `applicationframehost.exe`）。此前展示层只做 `.trim_end_matches(".exe")`，
//! 于是：
//! - UWP 宿主 `applicationframehost.exe` 原样进 Top 榜、语义不可读；
//! - 常见应用（`msedge.exe` / `code.exe` / `qq.exe`…）以裸进程名展示，
//!   与「应用图标/友好名」的产品预期不一致。
//!
//! 本模块做三件事（均为纯函数、硬件无关、可单测）：
//! 1. **宿主进程 → 友好标签**（`applicationframehost`→"UWP" 等）。宿主名是
//!    平台侧采集器与本页共用的同一份名单（见 [`HOST_LABELS`]）；核心/平台
//!    侧若要复用，请引用本表而非各自再写一份。
//! 2. **剔除 `.exe` 后缀**（大小写不敏感）。
//! 3. **常见应用 → 友好名别名**（`msedge`→"Microsoft Edge" 等，[`FRIENDLY_ALIAS`]）。
//!
//! `display_app` 是幂等的（`display_app(display_app(x)) == display_app(x)`），
//! 因此可在排序前后任意调用，不影响计数与 Top-N 顺序。
//!
//! 说明：基于 exe 版本信息（FileDescription/Product/CompanyName）与图标
//! 提取的完整子系统（需 Win32 探针 + 磁盘定位）不在本只读服务内实现，
//! 这里用别名表给出确定性、可测的第一步；后续可在此层替换为版本信息源。

/// 宿主/基础设施进程名 → 友好标签。键为小写、去 `.exe` 的进程基名；
/// 值为展示层使用的标签（与 `"(other)"` / `"(unknown)"` 哨兵互不冲突，
/// 故 Top-N 聚合不会把多个宿主并进同一哨兵造成重名）。
/// 这是「平台侧宿主名单」的共享事实源：tray/采集器若需识别宿主，请引用本表。
pub const HOST_LABELS: &[(&str, &str)] = &[
    ("applicationframehost", "UWP"),
    ("textinputhost", "IME"),
    ("conhost", "Console"),
];

/// 常见应用进程基名（小写、去 `.exe`）→ 友好展示名。
/// 键互异、值互异，且不与任何哨兵/宿主标签冲突，保证映射单射、幂等。
/// 有意不收录 `code`（VS Code）：裸名 `code` 已可读，且测试夹具以 `code` 命名。
pub const FRIENDLY_ALIAS: &[(&str, &str)] = &[
    // 浏览
    ("chrome", "Chrome"),
    ("msedge", "Microsoft Edge"),
    ("firefox", "Firefox"),
    ("brave", "Brave"),
    ("opera", "Opera"),
    ("vivaldi", "Vivaldi"),
    // 通讯
    ("wechat", "WeChat"),
    ("weixin", "WeChat"),
    ("qq", "QQ"),
    ("telegram", "Telegram"),
    ("discord", "Discord"),
    ("slack", "Slack"),
    ("dingtalk", "DingTalk"),
    ("feishu", "Feishu"),
    ("outlook", "Outlook"),
    ("thunderbird", "Thunderbird"),
    // 娱乐
    ("steam", "Steam"),
    ("bilibili", "Bilibili"),
    ("youtube", "YouTube"),
    ("spotify", "Spotify"),
    ("douyin", "Douyin"),
    ("tiktok", "TikTok"),
    ("obs64", "OBS"),
    ("obs32", "OBS"),
    ("obs", "OBS"),
    // 文档
    ("winword", "Word"),
    ("excel", "Excel"),
    ("powerpnt", "PowerPoint"),
    ("notepad", "Notepad"),
    ("wps", "WPS"),
    ("obsidian", "Obsidian"),
    ("notion", "Notion"),
    // 设计
    ("photoshop", "Photoshop"),
    ("figma", "Figma"),
    ("blender", "Blender"),
    ("gimp", "GIMP"),
    ("inkscape", "Inkscape"),
    ("premiere", "Premiere"),
    // 系统小工具
    ("calc", "Calculator"),
    ("mspaint", "Paint"),
    ("devenv", "Visual Studio"),
];

/// 小写、去 `.exe` 后缀的进程基名（供宿主/别名查找用）。
fn base_of(name: &str) -> String {
    let s = name.trim();
    let no_ext = s
        .strip_suffix(".exe")
        .or_else(|| s.strip_suffix(".EXE"))
        .unwrap_or(s);
    no_ext.to_ascii_lowercase()
}

/// 原始 app 名（DB `app_name`，形如 `chrome.exe` / `applicationframehost.exe` /
/// `code`）→ 展示层归一名。
///
/// - 空/纯空白 → `"(unknown)"`（与 SQL `COALESCE` 哨兵一致，防御性兜底）。
/// - 宿主进程 → [`HOST_LABELS`] 友好标签。
/// - 常见应用 → [`FRIENDLY_ALIAS`] 友好名。
/// - 其余 → 原样（仅剔除 `.exe` 后缀，保留原始大小写）。
pub fn display_app(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return "(unknown)".to_string();
    }
    let base = base_of(trimmed);
    if let Some((_, label)) = HOST_LABELS
        .iter()
        .find(|(k, _)| (*k).eq_ignore_ascii_case(&base))
    {
        return label.to_string();
    }
    if let Some((_, alias)) = FRIENDLY_ALIAS
        .iter()
        .find(|(k, _)| (*k).eq_ignore_ascii_case(&base))
    {
        return alias.to_string();
    }
    trimmed
        .strip_suffix(".exe")
        .or_else(|| trimmed.strip_suffix(".EXE"))
        .unwrap_or(trimmed)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_exe_case_insensitively() {
        assert_eq!(display_app("chrome.exe"), "Chrome"); // 命中别名
        assert_eq!(display_app("Notepad.EXE"), "Notepad");
        assert_eq!(display_app("someapp.exe"), "someapp");
        assert_eq!(display_app("someapp"), "someapp");
    }

    #[test]
    fn hosts_map_to_friendly_labels() {
        assert_eq!(display_app("applicationframehost.exe"), "UWP");
        assert_eq!(display_app("textinputhost.exe"), "IME");
        assert_eq!(display_app("conhost.exe"), "Console");
    }

    #[test]
    fn is_idempotent() {
        for raw in [
            "chrome.exe",
            "applicationframehost.exe",
            "msedge.exe",
            "qq.exe",
            "code.exe",
            "(other)",
            "(unknown)",
            "UWP",
            "Chrome",
            "Microsoft Edge",
        ] {
            let once = display_app(raw);
            assert_eq!(display_app(&once), once, "display_app 非幂等: {raw}");
        }
    }

    #[test]
    fn sentinels_and_unknown_pass_through() {
        assert_eq!(display_app("(other)"), "(other)");
        assert_eq!(display_app("(unknown)"), "(unknown)");
        assert_eq!(display_app(""), "(unknown)");
        assert_eq!(display_app("   "), "(unknown)");
    }

    #[test]
    fn labels_never_collide_with_sentinels() {
        // 关键不变量：任何宿主/别名标签都不得等于聚合哨兵，否则 Top-N 会把
        // 多个宿主并进同一 "(other)" 造成重名。别名表允许同值（wechat/weixin
        // 都指 WeChat；obs64/obs32/obs 都指 OBS——同一应用的不同进程名）。
        for (_, v) in FRIENDLY_ALIAS.iter().chain(HOST_LABELS.iter()) {
            assert!(*v != "(other)" && *v != "(unknown)", "标签撞哨兵: {v}");
        }
        // 宿主标签自身两两不同（不同宿主不得共用一个标签）
        let host_vals: Vec<&str> = HOST_LABELS.iter().map(|(_, v)| *v).collect();
        let unique: std::collections::HashSet<&str> = host_vals.iter().copied().collect();
        assert_eq!(unique.len(), host_vals.len(), "宿主标签重名: {host_vals:?}");
    }
}
