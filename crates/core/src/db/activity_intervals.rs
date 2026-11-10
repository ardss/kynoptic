//! activity_intervals 派生区间表（PRD v1.0 跨生态活动感知，Stage 1，migration 0014）
//!
//! 统一时间轴的区间数据来自本表；它是**只读派生缓存**（stage0-audit §3：不进
//! 真相链），任何时刻都可由 events 全量重建（[`rebuild_activity_intervals_at`]），
//! 因此 UI / CLI / MCP 三端只读本表即可保证数值一致（PRD DoD）。
//!
//! 重建口径（stage0-audit §2/§4）：
//! - foreground_dwell：按 window/switch 事件推导 (app_name, window_title) 去重
//!   区间。**连续同 (app, title) 只刷新 end、不产生新区间**——修复旧口径
//!   「相邻 switch 事件相减」被重复事件多算时长的问题（样本实测同题 1 秒内
//!   最多 3 条、每分钟整点一次刷新触发）。
//! - away：idle_start/idle_end 事件对推导；连续 idle_start 以新的为准
//!   （与 db/sessions.rs 同语义，丢一段无终点的计时、不猜）；尾部未闭合按
//!   重建时刻收口。
//! - quality：observed = 证据闭环；estimated = 区间与 away 相交（前台在场
//!   存疑，输入空闲但焦点仍在）——**只打标不裁剪**：estimated 段的
//!   duration_ms 含 away 内整段时长（stage0-audit §4 的「∩ 在场区间」按
//!   相交标记口径实现，不做净时长裁剪）；unknown = 断线收口（末段无后续
//!   switch 佐证，按重建时刻 now 收口——unknown 是真实缺口，不是 0）。
//! - tab_foreground_dwell：Stage 2 F02 扩展（tab_change 证据）接入后启用，
//!   当前 rebuild 不产出该 kind。
//! - project（F06 项目归属，Stage 3 + 0018 人工修正）：优先级两档——
//!   **人工指定 > 规则推断**：
//!   1. [`ProjectOverride`]（migration 0018 project_overrides 表，append-only
//!      人工指定记录）：区间 (kind, app_name, title_hash) 相同且窗口相交即
//!      命中，多条取 id 最大（最晚指定）。project_source='manual'，归属证据
//!      **observed**（用户亲口指定是直证）；不改变区间 quality 本身（那是
//!      在场证据，与归属证据两回事）。
//!   2. [`ProjectRule`]（路径前缀/标题关键词 → 项目名）：无人工指定覆盖时
//!      按区间标题匹配。project_source='rule'，证据 inferred（规则匹配 ≠
//!      直证）。
//!   3. 全不命中 → project NULL、project_source NULL（无归属证据，不猜；
//!      查询侧按 unassigned 展示）。规则来自 settings.json 的 `project_rules`
//!      （opt-in，默认空列表 = 不打标）。

use rusqlite::{params, Connection};

/// 区间种类：前台驻留
pub const KIND_FOREGROUND_DWELL: &str = "foreground_dwell";
/// 区间种类：浏览器 tab 前台驻留（Stage 2 F02 预留）
pub const KIND_TAB_FOREGROUND_DWELL: &str = "tab_foreground_dwell";
/// 区间种类：离场
pub const KIND_AWAY: &str = "away";
/// 区间种类：AI 会话运行（Stage 2 F05，从 ai_session 事件推导）
pub const KIND_AI_RUN: &str = "ai_run";
/// 区间种类：编辑器前台会话（Stage 2 F04，编辑器前台驻留的分类视图）
pub const KIND_EDITOR_SESSION: &str = "editor_session";

/// project 字段的证据等级：规则匹配是推导（PRD §2.3 inferred），不是直证
pub const PROJECT_EVIDENCE: &str = "inferred";
/// project_source 词表：人工指定（migration 0018 project_overrides，证据
/// observed——用户亲口指定是最强证据）
pub const PROJECT_SOURCE_MANUAL: &str = "manual";
/// project_source 词表：规则命中（settings project_rules，证据 inferred）
pub const PROJECT_SOURCE_RULE: &str = "rule";
/// project_source 词表：无归属（project NULL；查询侧展示用，库里存 NULL
/// 不存该串——无证据不虚构字段值）
pub const PROJECT_SOURCE_UNASSIGNED: &str = "unassigned";

// ─── 进程内规则快照（独立复审 2026-11-03 生产接线）────────────────────────
//
// rebuild 需要 project_rules，而规则权威源是 settings.json（dash/cli 各有
// loader，core 不能反向依赖）。core 提供进程内快照：tray 启动与 settings.json
// mtime 变化时推送，dash POST /api/settings 保存成功后同步推送（同一进程，
// tray 内嵌 dashboard 时即时生效；独立 dashboard 进程由 tray 的 mtime 监视
// 兜底）。collector writer 节流重建时读取该快照——规则保存后在一个重建
// 周期内生效，不再「落盘即死」。

static CURRENT_PROJECT_RULES: std::sync::OnceLock<std::sync::RwLock<Vec<ProjectRule>>> =
    std::sync::OnceLock::new();

/// 推送当前进程的 project_rules 快照（全量替换；空列表 = opt-in 默认不打标）。
pub fn set_current_project_rules(rules: Vec<ProjectRule>) {
    let lock = CURRENT_PROJECT_RULES.get_or_init(|| std::sync::RwLock::new(Vec::new()));
    if let Ok(mut g) = lock.write() {
        *g = rules;
    }
}

/// 读取当前 project_rules 快照（重建调用方使用；未推送过 → 空规则）。
pub fn current_project_rules() -> Vec<ProjectRule> {
    CURRENT_PROJECT_RULES
        .get_or_init(|| std::sync::RwLock::new(Vec::new()))
        .read()
        .map(|g| g.clone())
        .unwrap_or_default()
}

/// F06 项目归属规则（settings.json `project_rules`，opt-in 默认空）。
/// 匹配顺序即优先级，第一条命中的规则决定区间 project。
/// 字段语义（与阶段任务口径一致）：
/// - `path_prefix`：窗口标题以该前缀开头（大小写不敏感）即命中——编辑器
///   标题典型形态是「文件路径 - 编辑器名」；
/// - `title_keywords`：空格分隔关键词，任一关键词为标题子串即命中
///   （CJK 关键词按子串语义，ASCII 亦子串——项目名常为短词，词边界
///   语义反而漏配；与 CategoryRule 的双语义不同，这里刻意全部子串，
///   规则由用户显式配置，误配面由配置承担）。
///
/// 两个字段都为空的规则不命中任何区间（防呆）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ProjectRule {
    /// 项目名（写入 activity_intervals.project 的值）
    pub name: String,
    /// 路径前缀（标题开头匹配，大小写不敏感；空 = 不按前缀匹配）
    #[serde(default)]
    pub path_prefix: String,
    /// 空格分隔标题关键词（子串匹配，大小写不敏感；空 = 不按关键词匹配）
    #[serde(default)]
    pub title_keywords: String,
}

impl ProjectRule {
    /// 标题是否命中本规则（大小写不敏感；前缀或任一关键词命中即 true）。
    pub fn matches_title(&self, title: &str) -> bool {
        let title_lc = title.to_lowercase();
        let prefix = self.path_prefix.trim();
        if !prefix.is_empty() && title_lc.starts_with(&prefix.to_lowercase()) {
            return true;
        }
        self.title_keywords
            .split_whitespace()
            .any(|kw| title_lc.contains(&kw.to_lowercase()))
    }
}

/// 按规则列表给标题定项目归属：第一条命中的规则名；全不命中 → None。
pub fn project_of(rules: &[ProjectRule], title: &str) -> Option<String> {
    rules
        .iter()
        .find(|r| r.matches_title(title))
        .map(|r| r.name.clone())
}

// ─── 人工项目指定（migration 0018 project_overrides，F06 缺口补齐）──────────

/// project_overrides 一行：人工项目指定记录（append-only 派生层）。
///
/// 语义见 migration 0018 与本模块头注释：修正再指定 = 追加新行，永不
/// UPDATE/DELETE；rebuild 时按 (kind, app_name, title_hash) + 窗口相交匹配，
/// id 最大（最晚指定）者胜——追加覆盖。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectOverride {
    /// 区间种类（foreground_dwell / editor_session / ai_run …）
    pub interval_kind: String,
    /// 指定窗口起点（UTC RFC3339）
    pub start_ts: String,
    /// 指定窗口终点（UTC RFC3339）
    pub end_ts: String,
    /// 区间 app_name（精确匹配；ai_run 的 app_name = 源名）
    pub app_name: String,
    /// 区间 title_hash（NULL = 匹配无标题区间，如 ai_run）
    pub title_hash: Option<String>,
    /// 人工指定的项目名
    pub project: String,
    /// 指定前的归属来源（'rule' / 'manual' / 'unassigned'，审计）
    pub corrected_from: String,
}

/// 读出全部人工指定（append-only 全量读取；多条命中由
/// [`manual_project_of`] 取 id 最大者，故此处连同 id 一起返回）。
pub fn load_project_overrides(conn: &Connection) -> rusqlite::Result<Vec<(i64, ProjectOverride)>> {
    let mut stmt = conn.prepare(
        "SELECT id, interval_kind, start_ts, end_ts, app_name, title_hash, project, corrected_from \
         FROM project_overrides ORDER BY id",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, String>(4)?,
            r.get::<_, Option<String>>(5)?,
            r.get::<_, String>(6)?,
            r.get::<_, String>(7)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (id, kind, start_ts, end_ts, app_name, title_hash, project, corrected_from) = row?;
        out.push((
            id,
            ProjectOverride {
                interval_kind: kind,
                start_ts,
                end_ts,
                app_name,
                title_hash,
                project,
                corrected_from,
            },
        ));
    }
    Ok(out)
}

/// 归属三档的统一出口（rebuild 用）：返回 (project, project_source)。
/// - 人工指定命中（窗口相交 + kind/app/title_hash 相同，多条取 id 最大）
///   → (指定项目, manual)——**优先于规则**；
/// - 否则规则命中 → (规则项目, rule)；
/// - 全不命中 → (None, None)（无证据不猜；查询侧按 unassigned 展示）。
#[allow(clippy::too_many_arguments)] // 区间五元组 + 两路规则源，逐参直读比打包结构更省一次映射
pub fn attribute_project(
    overrides: &[(i64, ProjectOverride)],
    rules: &[ProjectRule],
    kind: &str,
    app: &str,
    title_hash: Option<&str>,
    title: Option<&str>,
    start_ms: i64,
    end_ms: i64,
) -> (Option<String>, Option<&'static str>) {
    // 窗口相交 + 标识三元组相同；title_hash 双方一致（None 只匹配 None）。
    // 取 id 最大（ORDER BY id 遍历顺序下 latter wins）。
    let mut manual: Option<&ProjectOverride> = None;
    for (_, ov) in overrides {
        if ov.interval_kind != kind || ov.app_name != app || ov.title_hash.as_deref() != title_hash
        {
            continue;
        }
        let ov_start = chrono::DateTime::parse_from_rfc3339(&ov.start_ts)
            .map(|t| t.timestamp_millis())
            .unwrap_or(0);
        let ov_end = chrono::DateTime::parse_from_rfc3339(&ov.end_ts)
            .map(|t| t.timestamp_millis())
            .unwrap_or(0);
        if start_ms < ov_end && ov_start < end_ms {
            manual = Some(ov); // id 递增遍历：后写覆盖先写（追加覆盖）
        }
    }
    if let Some(ov) = manual {
        return (Some(ov.project.clone()), Some(PROJECT_SOURCE_MANUAL));
    }
    let rule_hit = title.and_then(|t| project_of(rules, t));
    match rule_hit {
        Some(p) => (Some(p), Some(PROJECT_SOURCE_RULE)),
        None => (None, None),
    }
}

/// 写入一条人工指定（dash POST /api/intervals/project 与 CLI 共用；
/// `now_ms` 注入时钟）。append-only：只 INSERT，从不改旧行。`corrected_from`
/// 由调用方按指定前的实际归属如实传入（审计字段，不猜测）。
pub fn insert_project_override(
    conn: &Connection,
    ov: &ProjectOverride,
    created_at_ms: i64,
    created_by: &str,
    note: Option<&str>,
) -> rusqlite::Result<i64> {
    let created_at = rfc3339(created_at_ms);
    conn.execute(
        "INSERT INTO project_overrides \
         (interval_kind, start_ts, end_ts, app_name, title_hash, project, corrected_from, source, created_at, created_by, note) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            ov.interval_kind,
            ov.start_ts,
            ov.end_ts,
            ov.app_name,
            ov.title_hash,
            ov.project,
            ov.corrected_from,
            PROJECT_SOURCE_MANUAL,
            created_at,
            created_by,
            note,
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

/// 编辑器进程识别名单（F04：app_name 小写匹配；Stage 2 只做前台分类，
/// 不做编辑器内部扩展/插件接入——那是后续边界）。
const EDITOR_APPS: &[&str] = &[
    "code.exe",
    "code - insiders.exe",
    "cursor.exe",
    "windsurf.exe",
    "devenv.exe",
    "sublime_text.exe",
    "idea64.exe",
    "pycharm64.exe",
    "webstorm64.exe",
    "rider64.exe",
    "clion64.exe",
    "goland64.exe",
    "trae.exe",
];

fn is_editor_app(app: &str) -> bool {
    EDITOR_APPS.iter().any(|e| app.eq_ignore_ascii_case(e))
}

/// production 入口：真实时钟 → 注入式收口。
pub fn rebuild_activity_intervals(conn: &Connection) -> rusqlite::Result<usize> {
    rebuild_activity_intervals_at(conn, chrono::Utc::now().timestamp_millis())
}

/// 注入时钟版全量重建（`now_ms` = UTC epoch 毫秒）：测试零真实时钟依赖
/// （红线「测试不得读真实时钟」），生产行为与 [`rebuild_activity_intervals`]
/// 完全一致。返回写入的区间行数。
pub fn rebuild_activity_intervals_at(conn: &Connection, now_ms: i64) -> rusqlite::Result<usize> {
    rebuild_activity_intervals_with_rules_at(conn, now_ms, &[])
}

/// 带 F06 项目规则的重建变体：`rules` 为 settings.json `project_rules` 的
/// 预解析列表（调用方从设置文件读出）。空列表 = 不打 project（与
/// [`rebuild_activity_intervals_at`] 等价）。规则为空字段/空列表都宽容。
pub fn rebuild_activity_intervals_with_rules_at(
    conn: &Connection,
    now_ms: i64,
    rules: &[ProjectRule],
) -> rusqlite::Result<usize> {
    let switches = load_switches(conn)?;
    let away = derive_away_intervals(conn, now_ms);
    let dwell = derive_dwell_intervals(&switches, now_ms, &away);
    let ai_runs = derive_ai_run_intervals(conn);
    let ext_editors = derive_ext_editor_intervals(conn, now_ms);
    // F06 人工指定（0018）：append-only 表，rebuild 只读消费——人工指定
    // 优先于规则推断（attribute_project 内置优先级）。
    let overrides = load_project_overrides(conn)?;

    conn.execute_batch("BEGIN IMMEDIATE")?;
    let outcome = (|| -> rusqlite::Result<usize> {
        // 派生表：先清后建，全量口径，无增量状态
        conn.execute("DELETE FROM activity_intervals", [])?;
        let mut stmt = conn.prepare(
            "INSERT INTO activity_intervals \
             (interval_kind, start_ts, end_ts, app_name, title_hash, quality, source_id, project, project_source, device_id) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        )?;
        let mut n = 0usize;
        for iv in &dwell {
            let th = title_hash(&iv.title);
            let (project, psrc) = attribute_project(
                &overrides,
                rules,
                KIND_FOREGROUND_DWELL,
                &iv.app,
                Some(&th),
                Some(&iv.title),
                iv.start_ms,
                iv.end_ms,
            );
            n += stmt.execute(params![
                KIND_FOREGROUND_DWELL,
                rfc3339(iv.start_ms),
                rfc3339(iv.end_ms),
                iv.app,
                th,
                iv.quality,
                iv.source.as_deref().unwrap_or("windows"),
                project,
                psrc,
                iv.device.as_deref(),
            ])?;
        }
        for iv in &away {
            n += stmt.execute(params![
                KIND_AWAY,
                rfc3339(iv.start_ms),
                rfc3339(iv.end_ms),
                Option::<String>::None,
                Option::<String>::None,
                iv.quality,
                "windows",
                Option::<String>::None, // away 无应用/标题，无归属证据
                Option::<&str>::None,
                iv.device.as_deref(),
            ])?;
        }
        for iv in &ai_runs {
            // ai_session 事件不含标题/路径（隐私铁律：解析结果不带标题/正文），
            // 规则无从命中；但人工指定（0018）按 source 名 + 窗口相交仍可
            // 命中（用户显式指定 = 有证据）。
            let (project, psrc) = attribute_project(
                &overrides,
                rules,
                KIND_AI_RUN,
                &iv.source,
                None,
                None,
                iv.start_ms,
                iv.end_ms,
            );
            n += stmt.execute(params![
                KIND_AI_RUN,
                rfc3339(iv.start_ms),
                rfc3339(iv.end_ms),
                iv.source,
                Option::<String>::None,
                iv.quality,
                "ai",
                project,
                psrc,
                iv.device.as_deref(),
            ])?;
        }
        // F04：编辑器前台驻留的 editor_session 分类视图（从 dwell 直接映射，
        // quality 沿用 dwell 口径——闭区间 observed，断线收口尾部 unknown）
        for iv in dwell.iter().filter(|d| is_editor_app(&d.app)) {
            let th = title_hash(&iv.title);
            let (project, psrc) = attribute_project(
                &overrides,
                rules,
                KIND_EDITOR_SESSION,
                &iv.app,
                Some(&th),
                Some(&iv.title),
                iv.start_ms,
                iv.end_ms,
            );
            n += stmt.execute(params![
                KIND_EDITOR_SESSION,
                rfc3339(iv.start_ms),
                rfc3339(iv.end_ms),
                iv.app,
                th,
                iv.quality,
                "windows",
                project,
                psrc,
                iv.device.as_deref(),
            ])?;
        }
        // F04：编辑器扩展（vscode-extension / Cursor）经 dash /api/ext/events
        // 摄入的 editor 事件 → editor_session 直证区间（source_id =
        // 'vscode_ext'/'cursor_ext'，扩展直接观察焦点/文件/调试状态，证据等级
        // observed；尾部未闭合按重建时刻收口 unknown）。窗口名分类不等价问题
        // 的正解：扩展事件比窗口标题推断精确，两路证据并存、不互删。
        for iv in &ext_editors {
            let th = title_hash(&iv.title);
            let (project, psrc) = attribute_project(
                &overrides,
                rules,
                KIND_EDITOR_SESSION,
                &iv.editor,
                Some(&th),
                Some(&iv.title),
                iv.start_ms,
                iv.end_ms,
            );
            n += stmt.execute(params![
                KIND_EDITOR_SESSION,
                rfc3339(iv.start_ms),
                rfc3339(iv.end_ms),
                iv.editor,
                th,
                iv.quality,
                iv.source,
                project,
                psrc,
                iv.device.as_deref(),
            ])?;
        }
        Ok(n)
    })();
    match outcome {
        Ok(n) => {
            conn.execute_batch("COMMIT")?;
            Ok(n)
        }
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(e)
        }
    }
}

// ─── 区间推导（纯函数，测试直接覆盖）───────────────────────────────────────

#[derive(Debug, PartialEq)]
struct Interval {
    start_ms: i64,
    end_ms: i64,
    quality: &'static str,
    /// F11：源事件的 device_id 原样抄写；None = 事件未带设备标识（不虚构）
    device: Option<String>,
}

#[derive(Debug, PartialEq)]
struct DwellInterval {
    start_ms: i64,
    end_ms: i64,
    app: String,
    title: String,
    quality: &'static str,
    source: Option<String>,
    /// F11：首现事件的 device_id；同 (app,title) 刷新不改设备归属
    device: Option<String>,
}

struct RawSwitch {
    ts_ms: i64,
    app: String,
    title: String,
    source: Option<String>,
    device: Option<String>,
}

/// window/switch 事件按时间序读出（app_name 为空的行不参与前台区间推导）。
fn load_switches(conn: &Connection) -> rusqlite::Result<Vec<RawSwitch>> {
    let mut stmt = conn.prepare(
        "SELECT timestamp, app_name, window_title, source_id, device_id FROM events \
         WHERE event_type = 'window' AND event_action = 'switch' \
           AND app_name IS NOT NULL AND app_name != '' \
         ORDER BY timestamp, id",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, Option<String>>(2)?,
            r.get::<_, Option<String>>(3)?,
            r.get::<_, Option<String>>(4)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (ts, app, title, source, device) = row?;
        if let Ok(t) = chrono::DateTime::parse_from_rfc3339(&ts) {
            out.push(RawSwitch {
                ts_ms: t.timestamp_millis(),
                app,
                title: title.unwrap_or_default(),
                source,
                device,
            });
        }
    }
    Ok(out)
}

/// away 区间：idle_start/idle_end 事件对。device_id 取 idle_start 事件
/// （离场的起点证据），事件未带则 None（F11：不虚构本机标识）。
fn derive_away_intervals(conn: &Connection, now_ms: i64) -> Vec<Interval> {
    let mut stmt = match conn.prepare(
        "SELECT timestamp, event_action, device_id FROM events \
         WHERE event_action IN ('idle_start', 'idle_end') ORDER BY timestamp, id",
    ) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let rows: Vec<(i64, String, Option<String>)> = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
            ))
        })
        .map(|it| {
            it.filter_map(|r| r.ok())
                .filter_map(|(ts, action, device)| {
                    chrono::DateTime::parse_from_rfc3339(&ts)
                        .ok()
                        .map(|t| (t.timestamp_millis(), action, device))
                })
                .collect()
        })
        .unwrap_or_default();

    let mut out = Vec::new();
    let mut open: Option<(i64, Option<String>)> = None;
    for (ts, action, device) in rows {
        match action.as_str() {
            // 连续两个 idle_start：以新的为准（丢一段无终点的计时，不猜）
            "idle_start" => open = Some((ts, device)),
            "idle_end" => {
                if let Some((start, device)) = open.take() {
                    out.push(Interval {
                        start_ms: start,
                        end_ms: ts,
                        quality: "observed",
                        device,
                    });
                }
            }
            _ => {}
        }
    }
    // 尾部未闭合：按重建时刻收口（unknown——无 idle_end 佐证）
    if let Some((start, device)) = open {
        out.push(Interval {
            start_ms: start,
            end_ms: now_ms,
            quality: "unknown",
            device,
        });
    }
    out
}

/// foreground_dwell 去重区间：连续同 (app, title) 只刷新 end；不同 key 关闭
/// 当前区间并开新段。区间与 away **相交则标 estimated（只打标，不裁剪
/// duration）**——duration_ms 仍是完整去重区间时长，estimated 段含义是
/// 「该时长内在场存疑」，而非被 away 排除后的净时长；下游不得把 estimated
/// 段当作确定在场直接求和。尾部断线 → 按 now 收口标 unknown。
fn derive_dwell_intervals(
    switches: &[RawSwitch],
    now_ms: i64,
    away: &[Interval],
) -> Vec<DwellInterval> {
    let overlaps_away = |start_ms: i64, end_ms: i64| -> bool {
        away.iter()
            .any(|a| start_ms < a.end_ms && a.start_ms < end_ms)
    };
    let mut out: Vec<DwellInterval> = Vec::new();
    for s in switches {
        match out.last_mut() {
            // 连续同 (app, title)：只刷新，不产生新区间（重复事件去重口径）
            Some(cur) if cur.app == s.app && cur.title == s.title => {
                if s.ts_ms > cur.end_ms {
                    cur.end_ms = s.ts_ms;
                }
            }
            _ => {
                // 开新段前，把当前段按 away 相交补标记（out 非空时 last 已闭合）
                if let Some(cur) = out.last_mut() {
                    if cur.quality != "unknown" && overlaps_away(cur.start_ms, cur.end_ms) {
                        cur.quality = "estimated";
                    }
                }
                out.push(DwellInterval {
                    start_ms: s.ts_ms,
                    end_ms: s.ts_ms,
                    app: s.app.clone(),
                    title: s.title.clone(),
                    quality: "observed",
                    source: s.source.clone(),
                    device: s.device.clone(),
                });
            }
        }
    }
    // 尾部段：断线收口（无后续 switch 佐证）→ unknown + now；否则按 away 相交标记
    if let Some(cur) = out.last_mut() {
        if cur.end_ms < now_ms && !switches.is_empty() {
            let is_tail = switches
                .last()
                .map(|s| s.ts_ms == cur.end_ms && s.app == cur.app && s.title == cur.title)
                .unwrap_or(false);
            if is_tail {
                cur.end_ms = now_ms.max(cur.end_ms);
                cur.quality = "unknown";
            } else if overlaps_away(cur.start_ms, cur.end_ms) {
                cur.quality = "estimated";
            }
        } else if overlaps_away(cur.start_ms, cur.end_ms) {
            cur.quality = "estimated";
        }
    }
    out
}

/// source_id → 编辑器显示名（'vscode_ext' → 'vscode'，'cursor_ext' → 'cursor'）；
/// 未知来源剥掉 '_ext' 后缀原样保留（不猜，不回 'unknown'）。
fn ext_editor_name(source_id: &str) -> String {
    match source_id {
        "vscode_ext" => "vscode".to_string(),
        "cursor_ext" => "cursor".to_string(),
        s => s.strip_suffix("_ext").unwrap_or(s).to_string(),
    }
}

/// F04 编辑器扩展事件 → editor_session 区间（vscode-extension 摄入路径）。
///
/// events 表 event_type='editor' 的行（dash POST /api/ext/events 写入，
/// source_id ∈ {'vscode_ext','cursor_ext'}，window_title = 文件路径——与
/// F06 project_rules 的路径前缀匹配共用同一列）按时间序重放成区间：
/// - `focus_gained`/`window_focus`：焦点回到编辑器。已有同文件会话 → 只刷
///   end；不同文件/无会话 → 关旧段开新段。
/// - `file_open`/`document_change`：打开/编辑文件。同文件刷新 end，异文件
///   切段（文件切换本身是焦点证据）。
/// - `focus_lost`/`window_blur`：焦点离开，闭合当前段（observed——有明确的
///   离开证据，不猜测断线）。
/// - `debug_start`/`debug_end`：调试状态变化，属会话内活动，仅刷新 end；
///   无会话时忽略（无焦点证据不虚构会话）。
/// - 未闭合尾部按重建时刻收口，quality=unknown（真实缺口，不是 0）。
/// - 无文件路径的行（untitled 草稿等）window_title 为空：照常参与会话计时
///   （焦点证据仍真实），title_hash 哈希空串；project 规则不命中（无路径
///   证据，不猜归属）。
struct ExtEditorInterval {
    start_ms: i64,
    end_ms: i64,
    /// 编辑器显示名（vscode / cursor）
    editor: String,
    /// 文件路径（可为空串 = 无路径证据）
    title: String,
    quality: &'static str,
    /// source_id 原样（'vscode_ext' / 'cursor_ext'）
    source: String,
    device: Option<String>,
}

const EXT_EDITOR_ACTIONS: &[&str] = &[
    "focus_gained",
    "window_focus",
    "file_open",
    "document_change",
    "debug_start",
    "debug_end",
    "focus_lost",
    "window_blur",
];

/// 打开的扩展编辑器会话：(文件, 起点, 末次活动时刻, 首现事件 device_id, source_id)
type OpenExtSession = (String, i64, i64, Option<String>, String);

fn derive_ext_editor_intervals(conn: &Connection, now_ms: i64) -> Vec<ExtEditorInterval> {
    let mut stmt = match conn.prepare(
        "SELECT timestamp, event_action, window_title, source_id, device_id FROM events \
         WHERE event_type = 'editor' AND source_id IS NOT NULL \
         ORDER BY timestamp, id",
    ) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let rows: Vec<(i64, String, String, String, Option<String>)> = match stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?.unwrap_or_default(),
                r.get::<_, String>(3)?,
                r.get::<_, Option<String>>(4)?,
            ))
        })
        .map(|it| {
            it.filter_map(|r| r.ok())
                .filter_map(|(ts, action, title, source, device)| {
                    chrono::DateTime::parse_from_rfc3339(&ts)
                        .ok()
                        .map(|t| (t.timestamp_millis(), action, title, source, device))
                })
                .collect::<Vec<_>>()
        }) {
        Ok(it) => it
            .into_iter()
            .filter(|(_, action, ..)| EXT_EDITOR_ACTIONS.contains(&action.as_str()))
            .collect(),
        Err(_) => return Vec::new(),
    };

    let mut out: Vec<ExtEditorInterval> = Vec::new();
    let mut open: Option<OpenExtSession> = None;
    for (ts, action, title, source, device) in rows {
        match action.as_str() {
            // 焦点离开：闭合当前段（observed——有明确离开证据，不猜测断线）
            "focus_lost" | "window_blur" => {
                if let Some((title, start, _, dev, src)) = open.take() {
                    out.push(ExtEditorInterval {
                        start_ms: start,
                        end_ms: ts.max(start),
                        editor: ext_editor_name(&src),
                        title,
                        quality: "observed",
                        source: src,
                        device: dev,
                    });
                }
            }
            // 调试事件：会话内活动，仅推进末次活动时刻；无会话时忽略
            //（无焦点证据不虚构会话）。
            "debug_start" | "debug_end" => {
                if let Some(session) = open.as_mut() {
                    session.2 = session.2.max(ts);
                }
            }
            // 有文件的活动/焦点事件：同文件只推进末次活动时刻；异文件切段
            //（文件切换本身是焦点证据）；无会话则开新段。
            _ => match open.take() {
                Some((f, start, last, dev, src)) if f == title => {
                    open = Some((f, start, last.max(ts), dev, src));
                }
                Some((f, start, last, dev, src)) => {
                    out.push(ExtEditorInterval {
                        start_ms: start,
                        end_ms: last.max(start),
                        editor: ext_editor_name(&src),
                        title: f,
                        quality: "observed",
                        source: src,
                        device: dev,
                    });
                    open = Some((title, ts, ts, device, source));
                }
                None => {
                    open = Some((title, ts, ts, device, source));
                }
            },
        }
    }
    // 尾部未闭合：按重建时刻收口，quality=unknown（真实缺口，不是 0）
    if let Some((title, start, last, dev, src)) = open {
        out.push(ExtEditorInterval {
            start_ms: start,
            end_ms: now_ms.max(last).max(start),
            editor: ext_editor_name(&src),
            title,
            quality: "unknown",
            source: src,
            device: dev,
        });
    }
    out.sort_by_key(|i| i.start_ms);
    out
}

/// AI 会话区间（F05）：events 表 ai_session 事件按证据分两路还原为区间：
///
/// - **真实通道（observed，hook 摄取）**：ext_event_id 非空（ai_hooks /
///   /api/ext/events 写入）→ 按 (source, session_id) 分组，区间 =
///   [最早事件 ts, 最晚事件 ts]，quality=observed；
/// - **扫描后备（inferred，ai_adapter 摄取）**：event_data JSON 里
///   start/end 还原区间，quality 取最弱端证据（unknown > inferred >
///   observed）。
///
/// 不双计（并集口径铁律）：同 (source, session_id) 同时有两路证据时
/// **observed 优先、丢弃该会话的扫描区间**（hook 是亲眼收到的实时事件，
/// 比事后扫描证据强）；然后同源重叠/相邻区间做并集合并，merged quality
/// 取被合并区间中最弱者（最弱端定级惯例）。事件 start/end 不可解析的行
/// 丢弃——无时间证据不虚构区间。
struct AiRunInterval {
    start_ms: i64,
    end_ms: i64,
    source: String,
    quality: &'static str,
    /// F11：ai_session 事件行的 device_id（chatgpt_web 等远端来源也带各自
    /// 设备标识）；事件未带则 None。
    device: Option<String>,
}

/// (source, session_id) 去重键：observed 优先，不双计。
fn prefer_observed(
    map: &mut std::collections::HashMap<(String, String), AiRunInterval>,
    key: (String, String),
    iv: AiRunInterval,
) {
    match map.get_mut(&key) {
        // 双路并存：observed 赢（evidence 词表序 observed 最强；但扫描会话
        // 也可能标 observed——两端行级时间戳的 claude 扫描同为 observed，
        // 此时取并集（更宽窗口）即可，quality 已同为 observed）
        Some(existing) => {
            if iv.quality == "observed" && existing.quality != "observed" {
                *existing = iv;
            } else {
                // 取并集窗口，quality 取最弱
                existing.start_ms = existing.start_ms.min(iv.start_ms);
                existing.end_ms = existing.end_ms.max(iv.end_ms);
                if iv.quality == "unknown" {
                    existing.quality = "unknown";
                } else if iv.quality == "inferred" && existing.quality == "observed" {
                    existing.quality = "inferred";
                }
            }
        }
        None => {
            map.insert(key, iv);
        }
    }
}

fn derive_ai_run_intervals(conn: &Connection) -> Vec<AiRunInterval> {
    let mut stmt = match conn.prepare(
        "SELECT event_data, device_id, ext_event_id, timestamp FROM events \
         WHERE event_type = 'ai_session' AND event_data IS NOT NULL",
    ) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let rows = match stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, Option<String>>(1)?,
            r.get::<_, Option<String>>(2)?,
            r.get::<_, String>(3)?,
        ))
    }) {
        Ok(it) => it.filter_map(|r| r.ok()).collect::<Vec<_>>(),
        Err(_) => return Vec::new(),
    };
    let mut sessions: std::collections::HashMap<(String, String), AiRunInterval> =
        std::collections::HashMap::new();
    for (data, device, ext_id, ts) in rows {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&data) else {
            continue;
        };
        let source = v
            .get("source")
            .and_then(|s| s.as_str())
            .unwrap_or("unknown")
            .to_string();
        if ext_id.is_some() {
            // 真实通道（hook observed）：按会话聚合 [最早, 最晚] 事件 ts
            let session_id = v
                .get("session_id")
                .and_then(|s| s.as_str())
                .unwrap_or("unknown")
                .to_string();
            let start = chrono::DateTime::parse_from_rfc3339(&ts)
                .map(|t| t.timestamp_millis())
                .unwrap_or(0);
            if start <= 0 {
                continue;
            }
            let iv = AiRunInterval {
                start_ms: start,
                end_ms: start,
                source: source.clone(),
                quality: "observed",
                device: device.clone(),
            };
            let key = (source, session_id);
            // 会话窗口做并集扩张（同会话多条 hook 事件）
            prefer_observed(&mut sessions, key, iv);
            continue;
        }
        // 扫描后备：event_data start/end
        let parse = |key: &str| -> Option<i64> {
            match v.get(key) {
                Some(serde_json::Value::Number(n)) => n.as_i64(),
                Some(serde_json::Value::String(s)) => chrono::DateTime::parse_from_rfc3339(s)
                    .ok()
                    .map(|t| t.timestamp_millis()),
                _ => None,
            }
        };
        let (Some(start), Some(end)) = (parse("start"), parse("end")) else {
            continue;
        };
        let quality = match (
            v.get("start_quality").and_then(|q| q.as_str()),
            v.get("end_quality").and_then(|q| q.as_str()),
        ) {
            (Some("unknown"), _) | (_, Some("unknown")) => "unknown",
            (Some("inferred"), _) | (_, Some("inferred")) => "inferred",
            _ => "observed",
        };
        let session_id = v
            .get("session_id")
            .and_then(|s| s.as_str())
            .unwrap_or("unknown")
            .to_string();
        let iv = AiRunInterval {
            start_ms: start,
            end_ms: end.max(start),
            source: source.clone(),
            quality,
            device,
        };
        prefer_observed(&mut sessions, (source, session_id), iv);
    }
    // 同源重叠/相邻区间并集合并（merged quality = 最弱者）；device 取首个
    // （同一台机器上的同源会话 device 一致；跨设备不做跨源并集——device 不同
    // 的区间不合并，避免把两台机器的会话缝成一段假连续）。
    let mut by_source: std::collections::HashMap<(String, Option<String>), Vec<AiRunInterval>> =
        std::collections::HashMap::new();
    for ((source, _session), iv) in sessions {
        by_source
            .entry((source, iv.device.clone()))
            .or_default()
            .push(iv);
    }
    let mut out: Vec<AiRunInterval> = Vec::new();
    for (_, mut ivs) in by_source {
        ivs.sort_by_key(|i| i.start_ms);
        let mut merged: Vec<AiRunInterval> = Vec::new();
        for iv in ivs {
            match merged.last_mut() {
                Some(last) if iv.start_ms <= last.end_ms => {
                    last.end_ms = last.end_ms.max(iv.end_ms);
                    last.quality = weakest_quality(last.quality, iv.quality);
                }
                _ => merged.push(iv),
            }
        }
        out.extend(merged);
    }
    out.sort_by_key(|i| i.start_ms);
    out
}

/// 最弱端定级（unknown > inferred > observed）。
fn weakest_quality(a: &'static str, b: &str) -> &'static str {
    let rank = |q: &str| match q {
        "unknown" => 2,
        "inferred" => 1,
        _ => 0,
    };
    match rank(a).cmp(&rank(b)) {
        std::cmp::Ordering::Less => match b {
            "unknown" => "unknown",
            "inferred" => "inferred",
            _ => "observed",
        },
        _ => a, // a 不弱于 b
    }
}

/// window_title → 6 位十六进制短哈希（样本脱敏口径 title_h，如 9c2545）。
/// 哈希仅用于派生表分组合并展示，原始 title 仍在 events 表（唯一真相源）。
///
/// 算法 = FNV-1a 64-bit，**显式固定**（此前用 std `DefaultHasher`/SipHash，
/// Rust 官方不保证其跨版本稳定；本值持久化在 activity_intervals 表中，故
/// 换成不受 rustc 版本影响的显式算法，增量重建/跨版本对比不再漂移）。
fn title_hash(title: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325; // FNV offset basis
    for b in title.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3); // FNV prime
    }
    format!("{:06x}", h & 0xFF_FFFF)
}

fn rfc3339(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .unwrap_or(chrono::DateTime::UNIX_EPOCH)
        .to_rfc3339()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    /// 库连接：内存库每测试单独 open + 迁移，互不共享。
    fn migrated_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(crate::db::SCHEMA).unwrap();
        crate::db::run_migrations(&conn).unwrap();
        conn
    }

    /// 插入一条 window/switch 事件（ts 为相对 base 的秒偏移，统一 UTC RFC3339）。
    const BASE_MS: i64 = 1_792_977_600_000; // 2026-11-02T03:00:00Z，固定不读真实时钟

    fn ts(base_ms: i64, offset_secs: i64) -> String {
        rfc3339(base_ms + offset_secs * 1000)
    }

    fn insert_switch(conn: &Connection, base_ms: i64, offset_secs: i64, app: &str, title: &str) {
        conn.execute(
            "INSERT INTO events (timestamp, event_type, event_action, app_name, window_title) \
             VALUES (?1, 'window', 'switch', ?2, ?3)",
            params![ts(base_ms, offset_secs), app, title],
        )
        .unwrap();
    }

    fn insert_idle(conn: &Connection, base_ms: i64, offset_secs: i64, action: &str) {
        conn.execute(
            "INSERT INTO events (timestamp, event_type, event_action) VALUES (?1, 'system', ?2)",
            params![ts(base_ms, offset_secs), action],
        )
        .unwrap();
    }

    fn count_kind(conn: &Connection, kind: &str) -> i64 {
        conn.query_row(
            "SELECT COUNT(*) FROM activity_intervals WHERE interval_kind = ?1",
            params![kind],
            |r| r.get(0),
        )
        .unwrap()
    }

    fn dwell_secs(conn: &Connection, app: &str, title: &str) -> Vec<(i64, String)> {
        let mut stmt = conn
            .prepare(
                "SELECT (julianday(end_ts) - julianday(start_ts)) * 86400.0, quality \
                 FROM activity_intervals \
                 WHERE interval_kind='foreground_dwell' AND app_name=?1 AND title_hash=?2 \
                 ORDER BY start_ts",
            )
            .unwrap();
        // title_hash 与实现同源：直接查回存好的行
        let th = title_hash(title);
        stmt.query_map(params![app, th], |r| {
            Ok((
                (r.get::<_, f64>(0)? * 1000.0).round() as i64,
                r.get::<_, String>(1)?,
            ))
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
    }

    fn away_rows(conn: &Connection) -> Vec<(i64, String)> {
        let mut stmt = conn
            .prepare(
                "SELECT (julianday(end_ts) - julianday(start_ts)) * 86400.0, quality \
                 FROM activity_intervals WHERE interval_kind='away' ORDER BY start_ts",
            )
            .unwrap();
        stmt.query_map([], |r| {
            Ok((
                (r.get::<_, f64>(0)? * 1000.0).round() as i64,
                r.get::<_, String>(1)?,
            ))
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
    }

    /// 重复事件去重口径（stage0-audit §2）：同 (app,title) 连续刷新只产生
    /// 一个区间，时长 = 末次刷新 − 首次出现（相邻相减的旧口径会多算）。
    #[test]
    fn duplicate_same_title_events_produce_single_interval() {
        let conn = migrated_conn();
        // 0s 首现，1s/3s 同题重复，30s 每分钟整点刷新（样本真实模式）
        for off in [0, 1, 3, 30] {
            insert_switch(&conn, BASE_MS, off, "WindowsTerminal.exe", "doc");
        }
        let now = BASE_MS + 60_000;
        let n = rebuild_activity_intervals_at(&conn, now).unwrap();
        assert_eq!(count_kind(&conn, KIND_FOREGROUND_DWELL), 1);
        let rows = dwell_secs(&conn, "WindowsTerminal.exe", "doc");
        assert_eq!(rows.len(), 1);
        // 时长按断线收口 = now(60s) − 首现(0s)（末段无 switch 佐证，缺口如实入账）
        assert_eq!(rows[0].0, 60_000);
        assert_eq!(rows[0].1, "unknown"); // 断线收口：无后续 switch，按 now 收口
        assert_eq!(n, 1);
    }

    /// sample-30min.json 真实模式 fixture（11:00-11:02 段构造）：来回切换、
    /// 同题重复、每分钟整点刷新。断言区间数与时长符合 §4 新口径。
    #[test]
    fn sample_real_pattern_intervals_match_new_semantics() {
        let conn = migrated_conn();
        let switches: &[(i64, &str, &str)] = &[
            (0, "WindowsTerminal.exe", "9c2545"),
            (6, "chrome.exe", "c3f5fd"),
            (60, "WindowsTerminal.exe", "9c2545"),
            (63, "WindowsTerminal.exe", "9c2545"),
            (64, "WindowsTerminal.exe", "9c2545"),
            (64, "WindowsTerminal.exe", "fc0431"),
            (68, "WindowsTerminal.exe", "a1f52c"),
            (71, "WindowsTerminal.exe", "a1f52c"),
            (76, "WindowsTerminal.exe", "a1f52c"),
            (78, "WindowsTerminal.exe", "fc0431"),
            (79, "WindowsTerminal.exe", "fc0431"),
            (120, "WindowsTerminal.exe", "9c2545"), // 每分钟整点刷新
            (126, "chrome.exe", "c3f5fd"),
        ];
        for (off, app, title) in switches {
            insert_switch(&conn, BASE_MS, *off, app, title);
        }
        let now = BASE_MS + 130_000;
        rebuild_activity_intervals_at(&conn, now).unwrap();

        // 8 段连续同题 run（每段 = 相邻不同 (app,title) 边界切一次）
        assert_eq!(count_kind(&conn, KIND_FOREGROUND_DWELL), 8);

        // a1f52c 段：3 条同题重复 → 1 个区间 8s（旧口径相邻相减也是 8s，
        // 但 fc0431 双条 78/79 段旧口径会把中间重复算成 0 加上多算 1s；
        // 9c2545 整点刷新段 60-64 是本口径修复点）
        let a1 = dwell_secs(&conn, "WindowsTerminal.exe", "a1f52c");
        assert_eq!(a1.len(), 1);
        assert_eq!(a1[0].0, 8_000);
        assert_eq!(a1[0].1, "observed");

        // 9c2545 整点刷新段 60→64 = 4s
        let wt = dwell_secs(&conn, "WindowsTerminal.exe", "9c2545");
        assert_eq!(wt.len(), 3);
        assert_eq!(wt[1].0, 4_000);

        // 尾部 chrome：断线收口，end = now（130s），quality=unknown
        let chrome = dwell_secs(&conn, "chrome.exe", "c3f5fd");
        assert_eq!(chrome.len(), 2);
        assert_eq!(chrome[1].0, 4_000); // 126s → now(130s)
        assert_eq!(chrome[1].1, "unknown");
    }

    /// idle 交集口径（§4：foreground_dwell = 去重区间 ∩ 在场区间）：
    /// away 覆盖期间的前台区间标 estimated（焦点在但无输入，在场存疑）。
    #[test]
    fn dwell_overlapping_away_is_marked_estimated() {
        let conn = migrated_conn();
        insert_switch(&conn, BASE_MS, 0, "app.exe", "A");
        insert_switch(&conn, BASE_MS, 15, "app.exe", "A"); // 同题刷新 → A 段 [0,15s] 跨 away 起点
        insert_idle(&conn, BASE_MS, 10, "idle_start");
        insert_switch(&conn, BASE_MS, 20, "app.exe", "B"); // away 期间切窗（无输入证据）
        insert_switch(&conn, BASE_MS, 35, "app.exe", "B"); // 同题刷新 → B 段 [20,35s]
        insert_idle(&conn, BASE_MS, 40, "idle_end");
        insert_switch(&conn, BASE_MS, 50, "app.exe", "A");
        let now = BASE_MS + 60_000;
        rebuild_activity_intervals_at(&conn, now).unwrap();

        // away 区间完整落在库中
        let away = away_rows(&conn);
        assert_eq!(away.len(), 1);
        assert_eq!(away[0].0, 30_000);
        assert_eq!(away[0].1, "observed");

        // A 段 [0,15s] 与 away[10,40] 相交 → estimated
        let a = dwell_secs(&conn, "app.exe", "A");
        assert_eq!(a.len(), 2); // [0,15s] 与 [50s,now] 尾段
        assert_eq!(a[0].0, 15_000);
        assert_eq!(a[0].1, "estimated");
        assert_eq!(a[1].0, 10_000);
        assert_eq!(a[1].1, "unknown"); // [50s,60s] 尾段，断线收口优先

        // B 段 [20,35s] 整体落在 away 内 → estimated
        let b = dwell_secs(&conn, "app.exe", "B");
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].0, 15_000);
        assert_eq!(b[0].1, "estimated");
    }

    /// 断线收口（§2 样本 11:17:48 后零证据缺口）：尾部区间按重建时刻收口、
    /// 标 unknown——unknown 是真实缺口，不允许静默归 0。
    #[test]
    fn trailing_interval_is_closed_at_now_as_unknown() {
        let conn = migrated_conn();
        insert_switch(&conn, BASE_MS, 0, "app.exe", "only");
        let now = BASE_MS + 90_000;
        rebuild_activity_intervals_at(&conn, now).unwrap();
        let rows = dwell_secs(&conn, "app.exe", "only");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, 90_000); // 0s → now(90s)
        assert_eq!(rows[0].1, "unknown");
    }

    /// 幂等：重建两遍行集一致（派生表先清后建，无增量状态）。
    #[test]
    fn rebuild_is_idempotent() {
        let conn = migrated_conn();
        insert_switch(&conn, BASE_MS, 0, "app.exe", "x");
        insert_switch(&conn, BASE_MS, 5, "app.exe", "x");
        let now = BASE_MS + 30_000;
        let n1 = rebuild_activity_intervals_at(&conn, now).unwrap();
        let n2 = rebuild_activity_intervals_at(&conn, now).unwrap();
        assert_eq!(n1, n2);
        assert_eq!(count_kind(&conn, KIND_FOREGROUND_DWELL), 1);
        assert_eq!(
            count_kind(&conn, KIND_TAB_FOREGROUND_DWELL),
            0,
            "tab_foreground_dwell 为 Stage 2 F02 预留，扩展接入前不产出"
        );
    }

    /// F04：编辑器前台驻留（code.exe/cursor.exe）额外产出 editor_session
    /// 分类视图（source='windows'；闭区间 observed，尾部断线收口 unknown）；
    /// 非编辑器 app 不产出。
    #[test]
    fn editor_apps_get_editor_session_intervals() {
        let conn = migrated_conn();
        insert_switch(&conn, BASE_MS, 0, "Code.exe", "file"); // 大小写不敏感匹配
        insert_switch(&conn, BASE_MS, 10, "Code.exe", "file");
        insert_switch(&conn, BASE_MS, 20, "cursor.exe", "chat");
        insert_switch(&conn, BASE_MS, 28, "cursor.exe", "chat"); // 同题刷新
        insert_switch(&conn, BASE_MS, 30, "chrome.exe", "web");
        insert_switch(&conn, BASE_MS, 35, "Code.exe", "file2"); // 尾部编辑器段
        let now = BASE_MS + 40_000;
        rebuild_activity_intervals_at(&conn, now).unwrap();
        let editor: Vec<(String, i64, String)> = {
            let mut stmt = conn
                .prepare(
                    "SELECT app_name, (julianday(end_ts)-julianday(start_ts))*86400000.0, quality \
                     FROM activity_intervals WHERE interval_kind='editor_session' ORDER BY start_ts",
                )
                .unwrap();
            stmt.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    (r.get::<_, f64>(1)?).round() as i64,
                    r.get(2)?,
                ))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
        };
        assert_eq!(editor.len(), 3);
        assert_eq!(editor[0], ("Code.exe".into(), 10_000, "observed".into()));
        assert_eq!(editor[1], ("cursor.exe".into(), 8_000, "observed".into()));
        // 尾部 code 区间（35s→now）无后续 switch 佐证 → 与 dwell 同口径
        // unknown（未知不归零）
        assert_eq!(editor[2].0, "Code.exe");
        assert_eq!(editor[2].2, "unknown");
        // chrome（非编辑器）不产出 editor_session
        let non_editor: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM activity_intervals \
                 WHERE interval_kind='editor_session' AND app_name='chrome.exe'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(non_editor, 0);
    }

    /// F05：ai_session 事件（ai_adapter 摄入，event_data 含 start/end/quality）
    /// → ai_run 区间；无时间证据的事件行丢弃，不虚构区间。
    #[test]
    fn ai_session_events_derive_ai_run_intervals() {
        let conn = migrated_conn();
        let insert_ai = |start: &str, end: &str, sq: &str, eq: &str, source: &str| {
            conn.execute(
                "INSERT INTO events (timestamp, event_type, event_action, event_data, app_name, source_id, quality) \
                 VALUES (?1, 'ai_session', 'ai_run', ?2, ?3, 'ai', 'observed')",
                params![
                    start,
                    serde_json::json!({
                        "schema_version": 1,
                        "session_id": format!("fix-{source}-{sq}"),
                        "source": source,
                        "start": start,
                        "end": end,
                        "start_quality": sq,
                        "end_quality": eq,
                        "prompt_count": 1,
                    })
                    .to_string(),
                    source,
                ],
            )
            .unwrap();
        };
        let t = |off: i64| rfc3339(BASE_MS + off * 1000);
        insert_ai(&t(0), &t(120), "observed", "observed", "claude_code");
        insert_ai(&t(200), &t(260), "inferred", "inferred", "dsh");
        // 无时间证据的坏行：丢弃
        conn.execute(
            "INSERT INTO events (timestamp, event_type, event_action, event_data, app_name, source_id, quality) \
             VALUES (?1, 'ai_session', 'ai_run', ?2, 'zcode', 'ai', 'observed')",
            params![t(300), r#"{"schema_version":1,"session_id":"fix-bad","source":"zcode"}"#],
        )
        .unwrap();
        let now = BASE_MS + 400_000;
        rebuild_activity_intervals_at(&conn, now).unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT app_name, quality, (julianday(end_ts)-julianday(start_ts))*86400000.0 \
                 FROM activity_intervals WHERE interval_kind='ai_run' ORDER BY start_ts",
            )
            .unwrap();
        let rows: Vec<(String, String, i64)> = stmt
            .query_map([], |r| {
                Ok((r.get(0)?, r.get(1)?, (r.get::<_, f64>(2)?).round() as i64))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(rows.len(), 2, "无时间证据的事件行不得产出区间");
        assert_eq!(rows[0], ("claude_code".into(), "observed".into(), 120_000));
        assert_eq!(rows[1], ("dsh".into(), "inferred".into(), 60_000));
        // source_id='ai'
        let src: String = conn
            .query_row(
                "SELECT DISTINCT source_id FROM activity_intervals WHERE interval_kind='ai_run'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(src, "ai");
    }

    /// F05 真实通道（hook observed）：ext_event_id 非空的 ai_session 事件按
    /// (source, session_id) 聚合为 [最早, 最晚] 事件 ts 区间，quality=observed。
    #[test]
    fn hook_events_derive_observed_ai_run_intervals() {
        let conn = migrated_conn();
        let insert_hook = |ts_off: i64, source: &str, session: &str, ext_id: &str| {
            conn.execute(
                "INSERT INTO events (timestamp, event_type, event_action, event_data, app_name, source_id, quality, ext_event_id) \
                 VALUES (?1, 'ai_session', 'tool_call', ?2, ?3, ?3, 'observed', ?4)",
                params![
                    rfc3339(BASE_MS + ts_off * 1000),
                    serde_json::json!({"schema_version":1, "source": source, "session_id": session})
                        .to_string(),
                    source,
                    ext_id,
                ],
            )
            .unwrap();
        };
        insert_hook(0, "claude_code", "sess-hook-1", "claude_code:e1");
        insert_hook(30, "claude_code", "sess-hook-1", "claude_code:e2");
        insert_hook(90, "claude_code", "sess-hook-1", "claude_code:e3");
        // 另一会话不掺和
        insert_hook(200, "zcode", "sess-hook-2", "zcode:f1");
        rebuild_activity_intervals_at(&conn, BASE_MS + 400_000).unwrap();
        let rows: Vec<(String, i64, String)> = {
            let mut stmt = conn
                .prepare(
                    "SELECT app_name, (julianday(end_ts)-julianday(start_ts))*86400000.0, quality \
                     FROM activity_intervals WHERE interval_kind='ai_run' ORDER BY start_ts",
                )
                .unwrap();
            stmt.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    (r.get::<_, f64>(1)?).round() as i64,
                    r.get(2)?,
                ))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
        };
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0], ("claude_code".into(), 90_000, "observed".into()));
        assert_eq!(rows[1], ("zcode".into(), 0, "observed".into()));
    }

    /// F05 不双计（铁律）：同 (source, session_id) 两路证据并存（hook observed
    /// + 扫描 inferred）→ 只留 observed 区间，扫描区间不得再出一行。
    #[test]
    fn hook_observed_overrides_scan_for_same_session() {
        let conn = migrated_conn();
        // 扫描后备：inferred 会话（同 session_id）
        conn.execute(
            "INSERT INTO events (timestamp, event_type, event_action, event_data, app_name, source_id, quality) \
             VALUES (?1, 'ai_session', 'ai_run', ?2, 'claude_code', 'ai', 'inferred')",
            params![
                rfc3339(BASE_MS),
                serde_json::json!({
                    "schema_version": 1, "session_id": "sess-dual", "source": "claude_code",
                    "start": rfc3339(BASE_MS), "end": rfc3339(BASE_MS + 120_000),
                    "start_quality": "inferred", "end_quality": "inferred"
                })
                .to_string(),
            ],
        )
        .unwrap();
        // 真实通道：同 session_id 的 hook observed 事件
        for (off, ext) in [(30i64, "claude_code:d1"), (60, "claude_code:d2")] {
            conn.execute(
                "INSERT INTO events (timestamp, event_type, event_action, event_data, app_name, source_id, quality, ext_event_id) \
                 VALUES (?1, 'ai_session', 'tool_call', ?2, 'claude_code', 'claude_code', 'observed', ?3)",
                params![
                    rfc3339(BASE_MS + off * 1000),
                    serde_json::json!({"schema_version":1, "source":"claude_code", "session_id":"sess-dual"})
                        .to_string(),
                    ext,
                ],
            )
            .unwrap();
        }
        rebuild_activity_intervals_at(&conn, BASE_MS + 400_000).unwrap();
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM activity_intervals WHERE interval_kind='ai_run'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1, "同会话两路证据只留一行（observed 优先，不双计）");
        let (q, start, end): (String, String, String) = conn
            .query_row(
                "SELECT quality, start_ts, end_ts FROM activity_intervals WHERE interval_kind='ai_run'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(q, "observed", "observed 优先，丢弃 inferred 扫描区间");
        // 区间窗口 = hook observed 事件聚合（[30s,60s]），不是扫描的 [0,120]
        assert_eq!(start, rfc3339(BASE_MS + 30_000));
        assert_eq!(end, rfc3339(BASE_MS + 60_000));
    }

    /// F05 并集口径：同源同设备重叠区间合并为一行，merged quality 取最弱端。
    #[test]
    fn same_source_overlapping_ai_run_intervals_union_merge() {
        let conn = migrated_conn();
        let insert_hook = |off: i64, session: &str, ext: &str| {
            conn.execute(
                "INSERT INTO events (timestamp, event_type, event_action, event_data, app_name, source_id, quality, ext_event_id) \
                 VALUES (?1, 'ai_session', 'tool_call', ?2, 'claude_code', 'claude_code', 'observed', ?3)",
                params![
                    rfc3339(BASE_MS + off * 1000),
                    serde_json::json!({"schema_version":1, "source":"claude_code", "session_id":session})
                        .to_string(),
                    ext,
                ],
            )
            .unwrap();
        };
        // 会话 A [0,60]（hook 两事件）；会话 B [50,90] 重叠 → 并集 [0,90] 一行
        insert_hook(0, "sess-u1", "claude_code:u1");
        insert_hook(60, "sess-u1", "claude_code:u2");
        insert_hook(50, "sess-u2", "claude_code:u3");
        insert_hook(90, "sess-u2", "claude_code:u4");
        // 会话 C [200,260] 不重叠 → 独立一行
        insert_hook(200, "sess-u3", "claude_code:u5");
        insert_hook(260, "sess-u3", "claude_code:u6");
        // 会话 D 与 A 同源重叠但 device 不同 → 不跨设备缝合
        conn.execute(
            "INSERT INTO events (timestamp, event_type, event_action, event_data, app_name, source_id, quality, ext_event_id, device_id) \
             VALUES (?1, 'ai_session', 'tool_call', ?2, 'claude_code', 'claude_code', 'observed', ?3, 'dev-other')",
            params![
                rfc3339(BASE_MS + 40_000),
                serde_json::json!({"schema_version":1, "source":"claude_code", "session_id":"sess-u4"})
                    .to_string(),
                "claude_code:u7",
            ],
        )
        .unwrap();
        rebuild_activity_intervals_at(&conn, BASE_MS + 400_000).unwrap();
        let rows: Vec<(i64, i64, Option<String>)> = {
            let mut stmt = conn
                .prepare(
                    "SELECT (julianday(start_ts)-julianday(?))*86400000.0, \
                            (julianday(end_ts)-julianday(?))*86400000.0, device_id \
                     FROM activity_intervals WHERE interval_kind='ai_run' ORDER BY start_ts",
                )
                .unwrap();
            let base = rfc3339(BASE_MS);
            stmt.query_map(params![base, base], |r| {
                Ok((
                    (r.get::<_, f64>(0)?).round() as i64,
                    (r.get::<_, f64>(1)?).round() as i64,
                    r.get(2)?,
                ))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
        };
        assert_eq!(
            rows.len(),
            3,
            "重叠同源并集一行 + 不重叠会话一行 + 跨设备不缝合一行"
        );
        assert_eq!(rows[0], (0, 90_000, Option::<String>::None));
        assert_eq!(rows[1], (40_000, 40_000, Some("dev-other".into())));
        assert_eq!(rows[2], (200_000, 260_000, Option::<String>::None));
    }

    /// F06：project_rules 规则匹配（路径前缀/标题关键词）→ 区间 project
    /// 字段（推导标注，证据 inferred）；未命中 → NULL；空规则列表不打标。
    #[test]
    fn project_rules_tag_intervals_as_inferred() {
        let conn = migrated_conn();
        insert_switch(
            &conn,
            BASE_MS,
            0,
            "Code.exe",
            "K:/proj/alpha/src/main.rs - Visual Studio Code",
        );
        insert_switch(
            &conn,
            BASE_MS,
            10,
            "Code.exe",
            "K:/proj/alpha/src/main.rs - Visual Studio Code",
        );
        insert_switch(
            &conn,
            BASE_MS,
            20,
            "WindowsTerminal.exe",
            "beta-build (sample-project)",
        );
        insert_switch(&conn, BASE_MS, 30, "chrome.exe", "unrelated page");
        insert_switch(
            &conn,
            BASE_MS,
            40,
            "Code.exe",
            "gamma/notes.md - Visual Studio Code",
        );
        let now = BASE_MS + 50_000;
        let rules = vec![
            ProjectRule {
                name: "alpha".into(),
                path_prefix: "k:/proj/alpha".into(),
                title_keywords: String::new(),
            },
            ProjectRule {
                name: "beta".into(),
                path_prefix: String::new(),
                title_keywords: "sample-project".into(),
            },
        ];
        let n = rebuild_activity_intervals_with_rules_at(&conn, now, &rules).unwrap();
        assert!(n > 0);
        // (行存在, project)：away 行 app_name 恒 NULL，查不到行时外层 None
        let project_col = |kind: &str, app: &str| -> Option<Option<String>> {
            conn.query_row(
                "SELECT project FROM activity_intervals \
                 WHERE interval_kind=?1 AND app_name=?2 ORDER BY start_ts LIMIT 1",
                params![kind, app],
                |r| r.get::<_, Option<String>>(0),
            )
            .ok()
        };
        // 前缀命中 → alpha（dwell 与 editor_session 双视图同标）
        assert_eq!(
            project_col(KIND_FOREGROUND_DWELL, "Code.exe"),
            Some(Some("alpha".into()))
        );
        assert_eq!(
            project_col(KIND_EDITOR_SESSION, "Code.exe"),
            Some(Some("alpha".into()))
        );
        // 关键词命中 → beta
        assert_eq!(
            project_col(KIND_FOREGROUND_DWELL, "WindowsTerminal.exe"),
            Some(Some("beta".into()))
        );
        // 未命中 → 行存在但 project NULL（无归属证据，不猜）
        assert_eq!(project_col(KIND_FOREGROUND_DWELL, "chrome.exe"), Some(None));
        // away 无标题/无 app → 无该类行
        assert_eq!(project_col(KIND_AWAY, "Code.exe"), None);

        // 空 rules（rebuild_activity_intervals_at 默认路径）→ 全 NULL（opt-in 不打标）
        rebuild_activity_intervals_at(&conn, now).unwrap();
        assert_eq!(project_col(KIND_FOREGROUND_DWELL, "Code.exe"), Some(None));

        // 幂等：带规则重建两遍行集一致
        let n1 = rebuild_activity_intervals_with_rules_at(&conn, now, &rules).unwrap();
        let n2 = rebuild_activity_intervals_with_rules_at(&conn, now, &rules).unwrap();
        assert_eq!(n1, n2);
    }

    /// ProjectRule 匹配单元：前缀大小写不敏感、关键词子串、空字段防呆。
    #[test]
    fn project_rule_matches_title_semantics() {
        let r = ProjectRule {
            name: "p".into(),
            path_prefix: "K:/Proj".into(),
            title_keywords: "фикс sample".into(),
        };
        assert!(r.matches_title("k:/proj/x/a.rs - Code"));
        assert!(
            !r.matches_title("xk:/proj/x/a.rs - Code"),
            "前缀必须从标题开头"
        );
        assert!(r.matches_title("build sample output"));
        assert!(r.matches_title("фикс notes"));
        assert!(!r.matches_title("unrelated"));
        // 全空字段防呆：永不命中
        let empty = ProjectRule {
            name: "e".into(),
            path_prefix: String::new(),
            title_keywords: String::new(),
        };
        assert!(!empty.matches_title("anything"));
        // 第一条命中的规则优先
        let rules = vec![
            ProjectRule {
                name: "first".into(),
                path_prefix: String::new(),
                title_keywords: "x".into(),
            },
            ProjectRule {
                name: "second".into(),
                path_prefix: String::new(),
                title_keywords: "x".into(),
            },
        ];
        assert_eq!(project_of(&rules, "x file").as_deref(), Some("first"));
        assert_eq!(project_of(&rules, "nothing"), None);
    }

    /// F11：rebuild 把源事件的 device_id 原样抄写进 activity_intervals；
    /// 事件未带 device_id 时区间该列为 NULL（不虚构本机标识）。
    #[test]
    fn rebuild_carries_device_id_from_source_events() {
        let conn = migrated_conn();
        // 带 device_id 的 switch（脱敏 fixture：伪 UUID）
        conn.execute(
            "INSERT INTO events (timestamp, event_type, event_action, app_name, window_title, source_id, device_id) \
             VALUES (?1, 'window', 'switch', 'Code.exe', 'file', 'windows', 'dev-aaaa-1111')",
            params![ts(BASE_MS, 0)],
        )
        .unwrap();
        // 同题刷新（另一行，无 device_id）：区间设备归属仍取首现事件
        conn.execute(
            "INSERT INTO events (timestamp, event_type, event_action, app_name, window_title, source_id) \
             VALUES (?1, 'window', 'switch', 'Code.exe', 'file', 'windows')",
            params![ts(BASE_MS, 10)],
        )
        .unwrap();
        // 带 device_id 的 idle 对
        for (off, action) in [(20, "idle_start"), (40, "idle_end")] {
            conn.execute(
                "INSERT INTO events (timestamp, event_type, event_action, device_id) \
                 VALUES (?1, 'system', ?2, 'dev-aaaa-1111')",
                params![ts(BASE_MS, off), action],
            )
            .unwrap();
        }
        // ai_session 事件带 device_id
        conn.execute(
            "INSERT INTO events (timestamp, event_type, event_action, event_data, app_name, source_id, device_id) \
             VALUES (?1, 'ai_session', 'ai_run', ?2, 'chatgpt_web', 'ai', 'dev-bbbb-2222')",
            params![
                ts(BASE_MS, 50),
                r#"{"schema_version":1,"session_id":"fix-web","source":"chatgpt_web","start":"2026-11-02T03:00:50+00:00","end":"2026-11-02T03:01:20+00:00","start_quality":"observed","end_quality":"observed"}"#
            ],
        )
        .unwrap();
        let now = BASE_MS + 120_000;
        rebuild_activity_intervals_at(&conn, now).unwrap();

        let device_of = |kind: &str| -> Option<String> {
            conn.query_row(
                "SELECT device_id FROM activity_intervals WHERE interval_kind=?1 LIMIT 1",
                params![kind],
                |r| r.get::<_, Option<String>>(0),
            )
            .unwrap()
        };
        // dwell：首现事件的 device_id（同题刷新不改归属）
        assert_eq!(
            device_of(KIND_FOREGROUND_DWELL).as_deref(),
            Some("dev-aaaa-1111")
        );
        assert_eq!(
            device_of(KIND_EDITOR_SESSION).as_deref(),
            Some("dev-aaaa-1111")
        );
        // away：idle_start 事件的 device_id
        assert_eq!(device_of(KIND_AWAY).as_deref(), Some("dev-aaaa-1111"));
        // ai_run：事件行的 device_id（chatgpt_web 伪设备）
        assert_eq!(device_of(KIND_AI_RUN).as_deref(), Some("dev-bbbb-2222"));

        // 无 device_id 的事件 → 区间该列 NULL（不虚构）
        let conn2 = migrated_conn();
        insert_switch(&conn2, BASE_MS, 0, "app.exe", "x");
        rebuild_activity_intervals_at(&conn2, BASE_MS + 30_000).unwrap();
        let d: Option<String> = conn2
            .query_row(
                "SELECT device_id FROM activity_intervals WHERE interval_kind='foreground_dwell'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(d, None);
    }

    /// F04：编辑器扩展事件（event_type='editor'，source_id='vscode_ext'/
    /// 'cursor_ext'）直接派生 editor_session 区间：焦点/文件/调试事件重放
    /// 成段，闭段 observed；尾部未闭合按重建时刻收口 unknown；project 规则
    /// 按文件路径打标。窗口名分类（dwell 路径）与扩展直证并存不互删。
    #[test]
    fn ext_editor_events_derive_editor_session_intervals() {
        let conn = migrated_conn();
        let insert_ext = |off: i64, action: &str, title: Option<&str>, source: &str| {
            conn.execute(
                "INSERT INTO events (timestamp, event_type, event_action, app_name, window_title, source_id, quality, ext_event_id) \
                 VALUES (?1, 'editor', ?2, ?3, ?4, ?5, 'observed', ?5 || ':' || ?6)",
                params![
                    ts(BASE_MS, off),
                    action,
                    source.strip_suffix("_ext").unwrap_or(source),
                    title,
                    source,
                    off,
                ],
            )
            .unwrap();
        };
        // vscode：打开 fileA → 编辑刷新 → 焦点离开（闭段 observed）
        insert_ext(
            0,
            "file_open",
            Some("K:/proj/alpha/src/main.rs"),
            "vscode_ext",
        );
        insert_ext(
            10,
            "document_change",
            Some("K:/proj/alpha/src/main.rs"),
            "vscode_ext",
        );
        insert_ext(30, "focus_lost", None, "vscode_ext");
        // cursor：打开 fileB，无离开事件 → 尾部收口 unknown
        insert_ext(40, "file_open", Some("K:/proj/beta/notes.md"), "cursor_ext");
        // 调试事件：会话内活动（推进 last），无会话时忽略
        insert_ext(45, "debug_start", None, "cursor_ext");
        let now = BASE_MS + 60_000;
        let n = rebuild_activity_intervals_at(&conn, now).unwrap();
        let rows: Vec<(String, String, i64, String, String)> = {
            let mut stmt = conn
                .prepare(
                    "SELECT app_name, source_id, (julianday(end_ts)-julianday(start_ts))*86400000.0, quality, title_hash \
                     FROM activity_intervals WHERE interval_kind='editor_session' \
                       AND source_id IN ('vscode_ext','cursor_ext') ORDER BY start_ts",
                )
                .unwrap();
            stmt.query_map([], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    (r.get::<_, f64>(2)?).round() as i64,
                    r.get(3)?,
                    r.get(4)?,
                ))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
        };
        assert_eq!(rows.len(), 2, "扩展事件派生 2 段 editor_session");
        // A 段 [0,30s] observed，source_id 原样，app=vscode
        assert_eq!(rows[0].0, "vscode");
        assert_eq!(rows[0].1, "vscode_ext");
        assert_eq!(rows[0].2, 30_000);
        assert_eq!(rows[0].3, "observed");
        assert_eq!(rows[0].4, title_hash("K:/proj/alpha/src/main.rs"));
        // B 段 [40s, now] unknown（无焦点离开佐证，不猜）；app=cursor
        assert_eq!(rows[1].0, "cursor");
        assert_eq!(rows[1].1, "cursor_ext");
        assert_eq!(rows[1].2, 20_000);
        assert_eq!(rows[1].3, "unknown");
        assert_eq!(rows[1].4, title_hash("K:/proj/beta/notes.md"));
        // n 里除窗口 dwell/away 外应包含扩展 2 段（宽松断言：重建幂等）
        let n2 = rebuild_activity_intervals_at(&conn, now).unwrap();
        assert_eq!(n, n2);
    }

    /// F04：扩展编辑器会话按文件路径参与 F06 project_rules 打标（路径前缀
    /// 命中 → project；无路径 → NULL 不猜）。
    #[test]
    fn ext_editor_sessions_get_project_tags() {
        let conn = migrated_conn();
        conn.execute(
            "INSERT INTO events (timestamp, event_type, event_action, app_name, window_title, source_id, quality, ext_event_id) \
             VALUES (?1, 'editor', 'file_open', 'vscode', ?2, 'vscode_ext', 'observed', 'vscode_ext:e1')",
            params![ts(BASE_MS, 0), "K:/proj/alpha/src/lib.rs"],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO events (timestamp, event_type, event_action, app_name, window_title, source_id, quality, ext_event_id) \
             VALUES (?1, 'editor', 'focus_lost', 'vscode', NULL, 'vscode_ext', 'observed', 'vscode_ext:e2')",
            params![ts(BASE_MS, 20)],
        )
        .unwrap();
        let rules = vec![ProjectRule {
            name: "alpha".into(),
            path_prefix: "k:/proj/alpha".into(),
            title_keywords: String::new(),
        }];
        let now = BASE_MS + 40_000;
        rebuild_activity_intervals_with_rules_at(&conn, now, &rules).unwrap();
        let (project, quality, dur): (Option<String>, String, i64) = conn
            .query_row(
                "SELECT project, quality, (julianday(end_ts)-julianday(start_ts))*86400000.0 \
                 FROM activity_intervals WHERE interval_kind='editor_session' AND source_id='vscode_ext' LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, (r.get::<_, f64>(2)?).round() as i64)),
            )
            .unwrap();
        assert_eq!(project.as_deref(), Some("alpha"));
        assert_eq!(quality, "observed");
        assert_eq!(dur, 20_000);
    }

    /// migration 0014 存量回填：迁移前插入的旧事件在升级后
    /// source_id='windows'、quality='observed'（append-only：行数不变）。
    #[test]
    fn migration_0014_backfills_legacy_events() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(crate::db::SCHEMA).unwrap();
        // v1 存量库：先插入旧结构事件，再升级
        conn.execute(
            "INSERT INTO events (timestamp, event_type, event_action, app_name) \
             VALUES ('2026-11-01T10:00:00+00:00', 'window', 'switch', 'a.exe')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO metadata (key, value) VALUES ('schema_version', '1')",
            [],
        )
        .unwrap();
        crate::db::run_migrations(&conn).unwrap();
        let (total, with_source, with_quality): (i64, i64, i64) = conn
            .query_row(
                "SELECT COUNT(*), \
                 SUM(source_id='windows'), \
                 SUM(quality='observed') FROM events",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(total, 1);
        assert_eq!(with_source, 1, "存量行应回填 source_id='windows'");
        assert_eq!(with_quality, 1, "存量行应回填 quality='observed'");
        // 新写入事件不再被 UPDATE 覆盖显式值（幂等守卫 WHERE source_id IS NULL）
        conn.execute(
            "INSERT INTO events (timestamp, event_type, event_action, source_id, quality) \
             VALUES ('2026-11-01T10:01:00+00:00', 'window', 'switch', 'browser', 'confirmed')",
            [],
        )
        .unwrap();
        let kept: String = conn
            .query_row(
                "SELECT source_id FROM events WHERE timestamp='2026-11-01T10:01:00+00:00'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(kept, "browser", "显式值不得被回填覆盖");
    }

    // ─── F06 缺口补齐：人工指定（migration 0018 project_overrides）──────────

    fn insert_override(
        conn: &Connection,
        kind: &str,
        start_ms: i64,
        end_ms: i64,
        app: &str,
        th: Option<&str>,
        project: &str,
    ) -> i64 {
        insert_project_override(
            conn,
            &ProjectOverride {
                interval_kind: kind.into(),
                start_ts: rfc3339(start_ms),
                end_ts: rfc3339(end_ms),
                app_name: app.into(),
                title_hash: th.map(|s| s.into()),
                project: project.into(),
                corrected_from: "unassigned".into(),
            },
            BASE_MS + 120_000, // created_at 注入（不读真实时钟）
            "test",
            None,
        )
        .unwrap()
    }

    fn project_cols(conn: &Connection, kind: &str, app: &str) -> (Option<String>, Option<String>) {
        conn.query_row(
            "SELECT project, project_source FROM activity_intervals \
             WHERE interval_kind=?1 AND app_name=?2 ORDER BY start_ts LIMIT 1",
            params![kind, app],
            |r| {
                Ok((
                    r.get::<_, Option<String>>(0)?,
                    r.get::<_, Option<String>>(1)?,
                ))
            },
        )
        .unwrap()
    }

    /// 优先级 + 可追溯：人工指定（manual）胜过规则命中（rule）；规则命中
    /// 保持 rule/inferred；未命中 unassigned；全部 `_at` 注入时钟。
    #[test]
    fn manual_override_beats_rule_and_rule_hit_stays_inferred() {
        let conn = migrated_conn();
        insert_switch(&conn, BASE_MS, 0, "Code.exe", "k:/proj/alpha/src/main.rs");
        insert_switch(&conn, BASE_MS, 10, "Code.exe", "k:/proj/alpha/src/main.rs");
        insert_switch(
            &conn,
            BASE_MS,
            20,
            "WindowsTerminal.exe",
            "beta sample build",
        );
        insert_switch(&conn, BASE_MS, 30, "chrome.exe", "unrelated page");
        // 规则：alpha 前缀 → 规则项目；beta 关键词 → 规则项目
        let rules = vec![
            ProjectRule {
                name: "rule-alpha".into(),
                path_prefix: "k:/proj/alpha".into(),
                title_keywords: String::new(),
            },
            ProjectRule {
                name: "rule-beta".into(),
                path_prefix: String::new(),
                title_keywords: "beta".into(),
            },
        ];
        // 人工指定：alpha 区间窗口 [0, 30s] → manual 项目（优先于规则）
        let th = title_hash("k:/proj/alpha/src/main.rs");
        insert_override(
            &conn,
            KIND_FOREGROUND_DWELL,
            BASE_MS,
            BASE_MS + 30_000,
            "Code.exe",
            Some(&th),
            "manual-alpha",
        );
        let now = BASE_MS + 60_000;
        rebuild_activity_intervals_with_rules_at(&conn, now, &rules).unwrap();

        // 人工指定区间：manual 项目 + project_source=manual（优先于规则）
        let (p, s) = project_cols(&conn, KIND_FOREGROUND_DWELL, "Code.exe");
        assert_eq!(p.as_deref(), Some("manual-alpha"), "人工指定优先于规则");
        assert_eq!(s.as_deref(), Some(PROJECT_SOURCE_MANUAL));

        // 规则命中（无人工覆盖）：rule 项目 + project_source=rule（inferred 语义）
        let (p2, s2) = project_cols(&conn, KIND_FOREGROUND_DWELL, "WindowsTerminal.exe");
        assert_eq!(p2.as_deref(), Some("rule-beta"));
        assert_eq!(s2.as_deref(), Some(PROJECT_SOURCE_RULE));

        // 全不命中：project NULL、project_source NULL（查询侧 unassigned，不虚构）
        let (p3, s3) = project_cols(&conn, KIND_FOREGROUND_DWELL, "chrome.exe");
        assert_eq!(p3, None);
        assert_eq!(s3, None);
    }

    /// 追加覆盖：同区间两次人工指定 → id 最大（最晚）者生效；旧行保留
    /// （append-only：表行数只增不减，重建不动 project_overrides）。
    #[test]
    fn appended_override_supersedes_and_survives_rebuild() {
        let conn = migrated_conn();
        insert_switch(&conn, BASE_MS, 0, "Code.exe", "alpha file");
        insert_switch(&conn, BASE_MS, 10, "Code.exe", "alpha file");
        let th = title_hash("alpha file");
        insert_override(
            &conn,
            KIND_FOREGROUND_DWELL,
            BASE_MS,
            BASE_MS + 30_000,
            "Code.exe",
            Some(&th),
            "first-choice",
        );
        let id2 = insert_override(
            &conn,
            KIND_FOREGROUND_DWELL,
            BASE_MS,
            BASE_MS + 30_000,
            "Code.exe",
            Some(&th),
            "second-choice",
        );
        let now = BASE_MS + 60_000;
        let rules = vec![ProjectRule {
            name: "rule-x".into(),
            path_prefix: String::new(),
            title_keywords: "alpha".into(),
        }];
        let overrides_before: i64 = conn
            .query_row("SELECT COUNT(*) FROM project_overrides", [], |r| r.get(0))
            .unwrap();
        rebuild_activity_intervals_with_rules_at(&conn, now, &rules).unwrap();
        let (p, s) = project_cols(&conn, KIND_FOREGROUND_DWELL, "Code.exe");
        assert_eq!(
            p.as_deref(),
            Some("second-choice"),
            "追加覆盖：最晚指定生效"
        );
        assert_eq!(s.as_deref(), Some(PROJECT_SOURCE_MANUAL));
        // append-only：重建不改 project_overrides（行数不变；旧行未删）
        let overrides_after: i64 = conn
            .query_row("SELECT COUNT(*) FROM project_overrides", [], |r| r.get(0))
            .unwrap();
        assert_eq!(overrides_before, overrides_after);
        let kept: String = conn
            .query_row(
                "SELECT project FROM project_overrides WHERE id=?1",
                params![id2 - 1],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(kept, "first-choice", "旧行保留（append-only，不删数据）");
        // 幂等：重建两遍结果一致（可重建——人工指定在派生层重放）
        rebuild_activity_intervals_with_rules_at(&conn, now, &rules).unwrap();
        let (p2, _) = project_cols(&conn, KIND_FOREGROUND_DWELL, "Code.exe");
        assert_eq!(p2.as_deref(), Some("second-choice"));
    }

    /// 人工指定对 ai_run 生效（规则因无标题无法命中 ai_run；用户显式指定
    /// = 有证据），且不改变区间在场 quality（归属证据与在场证据两回事）。
    #[test]
    fn manual_override_applies_to_ai_run_without_touching_interval_quality() {
        let conn = migrated_conn();
        conn.execute(
            "INSERT INTO events (timestamp, event_type, event_action, event_data, app_name, source_id, quality) \
             VALUES (?1, 'ai_session', 'ai_run', ?2, 'claude_code', 'ai', 'observed')",
            params![
                rfc3339(BASE_MS),
                serde_json::json!({
                    "schema_version": 1, "session_id": "ov-1", "source": "claude_code",
                    "start": rfc3339(BASE_MS), "end": rfc3339(BASE_MS + 60_000),
                    "start_quality": "observed", "end_quality": "observed"
                })
                .to_string(),
            ],
        )
        .unwrap();
        // 人工指定 ai_run：app_name = 源名，title_hash NULL（无标题区间）
        insert_override(
            &conn,
            KIND_AI_RUN,
            BASE_MS,
            BASE_MS + 60_000,
            "claude_code",
            None,
            "manual-ai",
        );
        rebuild_activity_intervals_at(&conn, BASE_MS + 120_000).unwrap();
        let (p, s, q): (Option<String>, Option<String>, String) = conn
            .query_row(
                "SELECT project, project_source, quality FROM activity_intervals \
                 WHERE interval_kind='ai_run' LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(p.as_deref(), Some("manual-ai"));
        assert_eq!(s.as_deref(), Some(PROJECT_SOURCE_MANUAL));
        assert_eq!(q, "observed", "人工指定不改区间在场 quality");
    }

    /// editor_session 分类视图同样吃到人工指定（dwell 与 editor 视图同键
    /// 同窗 → 双视图同标 manual）。
    #[test]
    fn manual_override_covers_editor_session_view() {
        let conn = migrated_conn();
        insert_switch(&conn, BASE_MS, 0, "Code.exe", "proj file");
        insert_switch(&conn, BASE_MS, 10, "Code.exe", "proj file");
        let th = title_hash("proj file");
        insert_override(
            &conn,
            KIND_EDITOR_SESSION,
            BASE_MS,
            BASE_MS + 30_000,
            "Code.exe",
            Some(&th),
            "manual-edit",
        );
        rebuild_activity_intervals_at(&conn, BASE_MS + 60_000).unwrap();
        let (p, s) = project_cols(&conn, KIND_EDITOR_SESSION, "Code.exe");
        assert_eq!(p.as_deref(), Some("manual-edit"));
        assert_eq!(s.as_deref(), Some(PROJECT_SOURCE_MANUAL));
    }
}
