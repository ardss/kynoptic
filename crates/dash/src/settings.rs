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

/// 分类规则的加载钳制上限（挂账 Wave31：规则匹配 CPU 放大收口——手改
/// settings.json 塞进超量/超长 pattern 时，匹配成本随行数×规则数线性放大）。
pub const MAX_CATEGORIES: usize = 100;
pub const MAX_PATTERN_CHARS: usize = 200;

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

/// 窗口分类规则（正则，匹配 app_name 或 window_title，大小写不敏感）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CategoryRule {
    pub name: String,
    /// 不区分大小写的子串/正则模式（简化：按空格拆 token，任一 token 命中即归类）
    pub pattern: String,
    /// pattern 预编译（小写 token 化一次，加载时生成；序列化跳过）。
    /// 匹配热路径（每 dwell 段 × 每规则）不再重复 split/to_lowercase。
    #[serde(skip)]
    pub lc_tokens: Vec<String>,
}

impl CategoryRule {
    /// 由 name + pattern 构造并预编译（外部构造请走这里，保证 lc_tokens 同步）。
    pub fn new(name: impl Into<String>, pattern: impl Into<String>) -> Self {
        let mut r = Self {
            name: name.into(),
            pattern: pattern.into(),
            lc_tokens: Vec::new(),
        };
        r.recompile();
        r
    }

    /// pattern 变更后重建预编译 token（反序列化 skip 字段后也须调用）。
    pub fn recompile(&mut self) {
        self.lc_tokens = self
            .pattern
            .to_lowercase()
            .split_whitespace()
            .map(String::from)
            .collect();
    }

    pub(crate) fn rule(name: &str, pattern: &str) -> Self {
        Self::new(name, pattern)
    }

    /// app/title 是否命中该规则（token 子串匹配，大小写不敏感）。
    pub fn matches(&self, app: &str, title: &str) -> bool {
        // 每段只小写一次（与规则数无关），token 命中判定走预编译表
        let hay = format!("{} {}", app, title).to_lowercase();
        self.matches_lc(&hay)
    }

    /// [`matches`] 的小写预 映射版：调用方（如 report 的 classify 循环）对
    /// 同一段落逐一试规则时，可先 lowercase 一次再复用。
    pub fn matches_lc(&self, hay_lower: &str) -> bool {
        self.lc_tokens.iter().any(|tok| hay_lower.contains(tok))
    }
}

/// 加载后的钳制：categories 条数 ≤100、单条 pattern ≤200 字符（字符级截断），
/// 超限截断 + warn。POST 通道本就拒绝毒值，这里只兜手改文件的底。
fn clamp_categories(categories: &mut [CategoryRule]) {
    for r in categories.iter_mut() {
        if r.pattern.chars().count() > MAX_PATTERN_CHARS {
            log::warn!("categories[].pattern 超过 {MAX_PATTERN_CHARS} 字符，已截断");
            r.pattern = r.pattern.chars().take(MAX_PATTERN_CHARS).collect();
            r.recompile();
        }
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
    /// 人在场桥接阈值（分钟，0-15）：相邻真实键鼠分钟间隙不超过该值时按
    /// "无输入阅读"计入在场（审查 DeepSeek：2 分钟拍脑袋试点改为可配置）。
    #[serde(default = "default_presence_bridge_minutes")]
    pub presence_bridge_minutes: u32,
    /// 每键频次采集开关（默认开启：本地数据完整优先）：true（默认）= 键盘
    /// input_agg 事件附带 per-key `vk` 频次 map（只存每键次数，不存内容）；
    /// false = 显式 opt-out，不采集每键频次，只存每分钟计数。
    /// core 侧开关与 tray 接线由对应模块消费本字段。
    #[serde(default = "default_vk_frequency_enabled")]
    pub vk_frequency_enabled: bool,
    /// 窗口分类规则（正则 token，匹配顺序即优先级，未命中 → 其他）。
    #[serde(default = "default_categories")]
    pub categories: Vec<CategoryRule>,
}

fn default_daily_goal_minutes() -> u32 {
    480
}

fn default_presence_bridge_minutes() -> u32 {
    2
}

fn default_input_counts_only() -> bool {
    true
}

fn default_vk_frequency_enabled() -> bool {
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
            presence_bridge_minutes: 2,
            vk_frequency_enabled: true,
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
    // 极限注入审查：记事本"UTF-8 with BOM"保存会让 serde_json 解析失败，
    // 整份设置被静默判损坏回落默认。剥掉 UTF-8 BOM 再解析（仅此一处容错；
    // UTF-16 等其他编码仍按损坏处理，与 .corrupt.bak 兜底语义一致）。
    let raw = raw.strip_prefix('\u{feff}').unwrap_or(&raw);
    let mut s: AppSettings = serde_json::from_str(raw).ok()?;
    // 预编译 + 钳制（反序列化 skip 字段为空，必须重建；挂账 Wave31）
    let truncate_at = s.categories.len().min(MAX_CATEGORIES);
    if s.categories.len() > MAX_CATEGORIES {
        log::warn!("categories 条数超过 {MAX_CATEGORIES}，已截断");
        s.categories.truncate(MAX_CATEGORIES);
    }
    clamp_categories(&mut s.categories[..truncate_at]);
    for r in &mut s.categories {
        r.recompile();
    }
    Some(s)
}

/// 设置缓存：GET 侧热路径（每请求曾全量读盘 + serde 解析；坏文件场景还
/// 每次触发 rename 留档副作用）。以 (path, mtime, len) 失效——tray 等外部
/// 进程写文件后 mtime 变化即自动失效，无需跨进程信号。
struct SettingsCache {
    path: PathBuf,
    mtime: std::time::SystemTime,
    len: u64,
    settings: AppSettings,
}
static SETTINGS_CACHE: std::sync::Mutex<Option<SettingsCache>> = std::sync::Mutex::new(None);

fn stat_of(path: &Path) -> Option<(std::time::SystemTime, u64)> {
    let m = std::fs::metadata(path).ok()?;
    Some((m.modified().ok()?, m.len()))
}

/// 读取设置；文件不存在/损坏时返回缺省（autostart 先探测现有注册表
/// Run 项，与 `kynoptic-ctl autostart status` 同一事实源）。
/// 带进程内缓存：stat 未变化直接返回克隆，绝不重复读盘/解析。
pub fn load(db_path: &Path) -> AppSettings {
    let path = settings_path(db_path);
    let st = stat_of(&path);
    if let Ok(g) = SETTINGS_CACHE.lock() {
        if let Some(c) = g.as_ref() {
            if c.path == path && st == Some((c.mtime, c.len)) {
                return c.settings.clone();
            }
        }
    }
    let s = load_uncached(db_path, &path);
    // 文件存在的可缓存形态才落缓存（缺省路径含注册表探测，同样缓存：
    // 文件缺失时 stat 为 None，文件一旦出现 stat 变化即失效）。
    if let Ok(mut g) = SETTINGS_CACHE.lock() {
        if let Some((mtime, len)) = st {
            *g = Some(SettingsCache {
                path,
                mtime,
                len,
                settings: s.clone(),
            });
        } else if g.as_ref().map(|c| c.path.as_path()) == Some(path.as_path()) {
            *g = None; // 文件被删：丢弃旧缓存
        }
    }
    s
}

fn load_uncached(db_path: &Path, path: &Path) -> AppSettings {
    match try_load(db_path) {
        Some(s) => s,
        None => {
            // 文件存在但损坏：改名留档（审查 P1：坏文件若留原地，下次保存
            // 会用默认值静默覆盖用户配置；数据不删铁律，只改名）。缓存化后
            // 同一 stat 只触发一次 rename，不再每请求重复留档。
            if path.exists() {
                let bak = path.with_extension("json.corrupt.bak");
                let _ = std::fs::rename(path, &bak);
                log::warn!(
                    "settings.json 损坏，已留档为 {}，本次回退默认值",
                    bak.display()
                );
                // 兑现「改名留档并重建」承诺（cli/settings.rs 注释同口径）：
                // 留档后立即把回退的默认值落盘，否则 GET /api/settings 静默
                // 返回默认、文件却不存在，用户误以为设置已保存；直到下一次
                // save() 才重新出现。落盘失败仅记日志，不影响本次回退返回。
                let rebuilt = AppSettings {
                    autostart: autostart_registry_enabled(),
                    ..AppSettings::default()
                };
                if let Err(e) = save(db_path, &rebuilt) {
                    log::warn!("settings.json 重建失败：{}", e);
                }
                return rebuilt;
            }
            AppSettings {
                autostart: autostart_registry_enabled(),
                ..AppSettings::default()
            }
        }
    }
}

/// 写盘（原子性：先写 .tmp 再改名，避免半截 JSON）。
pub fn save(db_path: &Path, settings: &AppSettings) -> std::io::Result<()> {
    let path = settings_path(db_path);
    // 首次安装/数据目录晚于 settings 写出创建时，父目录可能尚不存在（os error 3）。
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // Wave20 P1：tmp 名带 pid+纳秒——此前固定名在"托盘启动同步写"与
    // "dash 线程 POST 写"并发时互相截断，rename 出半截 JSON → 全部配置
    // 静默回退默认。
    let uniq = format!(
        "json.tmp.{}.{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let tmp = path.with_file_name(uniq);
    let json = serde_json::to_string_pretty(settings).map_err(std::io::Error::other)?;
    std::fs::write(&tmp, json)?;
    // rename 失败（目标被杀软/备份独占、ACL 拒绝、磁盘满）必须清掉 tmp：
    // tmp 名每次调用带新纳秒，不清理会随每次失败永久泄漏一个文件。
    if let Err(e) = std::fs::rename(&tmp, &path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    // 写成功即失效缓存（下次 load 重新 stat 读盘）
    if let Ok(mut g) = SETTINGS_CACHE.lock() {
        if g.as_ref().map(|c| c.path.as_path()) == Some(path.as_path()) {
            *g = None;
        }
    }
    Ok(())
}

/// 校验 id 集合：全部必须在 MONITOR_REGISTRY 中。返回第一个非法 id。
pub fn first_invalid_id(ids: &[String]) -> Option<String> {
    ids.iter()
        .find(|id| !registry::all_monitor_ids().contains(&id.as_str()))
        .cloned()
}

/// 注册表 Run 项里 Kynoptic 是否已启用（缺省 autostart 的探测源）。
/// 仅 Windows 有实际意义；其他平台恒 false。
/// 注册表 Run 键探测的公开版（tray 启动同步用，见 tray/main.rs）。
#[cfg(target_os = "windows")]
pub fn autostart_registry_enabled_pub() -> bool {
    autostart_registry_enabled()
}

#[cfg(not(target_os = "windows"))]
pub fn autostart_registry_enabled_pub() -> bool {
    false
}

#[cfg(target_os = "windows")]
fn autostart_registry_enabled() -> bool {
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;
    const RUN_KEY_PATH: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
    // Wave40 挂账：Run 值按安装目录指纹命名（与安装器同算法，见 core::naming）
    let value_name = match kynoptic_core::naming::install_dir() {
        Some(d) => kynoptic_core::naming::run_value_name(&d),
        None => kynoptic_core::naming::LEGACY_RUN_VALUE_NAME.to_string(),
    };
    // 多实例/沙箱旁路（与 core/singleton.rs 的 KYNOPTIC_MUTEX_SUFFIX 同一
    // 开关）：副本实例的 settings.json 缺失时绝不继承宿主的注册表自启动
    // 状态——否则副本保存任意设置都会把宿主的 Run 键覆写为副本 exe 路径
    //（自启动劫持，平台审查 medium）。
    if std::env::var_os("KYNOPTIC_MUTEX_SUFFIX").is_some() {
        return false;
    }
    RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey(RUN_KEY_PATH)
        .and_then(|k| k.get_value::<String, _>(&value_name))
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
        assert!(s.vk_frequency_enabled, "每键频次默认必须开启");
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
            presence_bridge_minutes: 2,
            vk_frequency_enabled: false,
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
        // 注册表现状即缺省来源（可能为 true：托盘的设置同步会写 Run 项）
        assert_eq!(
            s.autostart,
            autostart_registry_enabled(),
            "autostart 缺省应与注册表 Run 项一致"
        );
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
