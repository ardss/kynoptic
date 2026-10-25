//! actor 归属查询层（0.4.0 F1/F2）——GPT 修订契约的权威实现。
//!
//! **证据语义（勘误 2）**：agent = 系统注入输入的代理指标
//! （evidence_type=injected_input），不宣称一定来自 AI Agent——辅助功能、
//! 宏程序同样产生注入输入。`ai_suspect` 是独立的合成推测指标，不是事实判定。
//!
//! **真值表（五类互斥完备）**：按分钟级证据 E_h/E_a（勘误 1 公式）+ 前台覆盖
//! E_f 判定：
//! - user       真人输入有直接证据（E_h=1 且 E_a=0）
//! - agent      注入输入有直接证据（E_a=1 且 E_h=0）
//! - mixed      真人+注入并存（E_h=1 且 E_a=1）
//! - unattended 前台有覆盖、无任何输入（E_f=1 且 E_h=0 且 E_a=0）
//! - unknown    无前台覆盖（采集空洞/首 switch 前），绝不硬塞其他类
//!
//! **归属规则（勘误 4）**：应用级秒数按前台区间与分钟的覆盖秒数分摊，命名
//! `estimated_actor_dwell_sec`（估算值，非精确 engaged）；同一分钟被多个区间
//! 覆盖时各区间按重叠秒数分配（区间构造上互不重叠，每分钟总分配 ≤60s）。
//! 停摆封顶沿用 `super::FG_DWELL_STALL_CAP_SECS`，且**不修改** 0.3.2 既有
//! `foreground_dwell_by_app` 口径；超过封顶的无观测区间按 unknown 处理。
//!
//! **缓存边界（勘误 3）**：`agg_minute_actor`（migration 0011）只存分钟级
//! 输入证据计数；本层的五类判定/channel/覆盖标签不落缓存。actor 查询的
//! 证据从 events 现算（epoch 分钟空间，规避 DST 回拨日本地钟面歧义——
//! presence 权威口径同此选择）；缓存与 events 的逐行等价由
//! `db::agg` 的重建/回填/增量三路径 + 等价性测试锁定。
//!
//! **channel 与 ai_suspect**：channel=browseros 由标题尾缀规则给出（默认
//! 「 - BrowserOS」，可配置），只是辅助线索列，永不改判五类；
//! ai_suspect = agent ∪ (browseros ∧ unattended)，输出时必须带推测标注。

use std::collections::HashMap;

use rusqlite::{params, Connection};

use super::{
    CLICKS_ROW_EXPR, INJECTED_CLICKS_ROW_EXPR, INJECTED_KEYS_ROW_EXPR, INJECTED_SCROLL_ROW_EXPR,
    KEYS_ROW_EXPR, SCROLL_ROW_EXPR,
};

/// 五类 actor 判定（互斥完备）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Actor {
    User,
    Agent,
    Unattended,
    Mixed,
    Unknown,
}

impl Actor {
    pub fn as_str(&self) -> &'static str {
        match self {
            Actor::User => "user",
            Actor::Agent => "agent",
            Actor::Unattended => "unattended",
            Actor::Mixed => "mixed",
            Actor::Unknown => "unknown",
        }
    }

    /// 由分钟证据判定（真值表）。`covered=false`（无前台覆盖）强制 unknown。
    pub fn from_evidence(human: bool, injected: bool, covered: bool) -> Actor {
        if !covered {
            return Actor::Unknown;
        }
        match (human, injected) {
            (true, false) => Actor::User,
            (false, true) => Actor::Agent,
            (true, true) => Actor::Mixed,
            (false, false) => Actor::Unattended,
        }
    }
}

/// AI 通道识别规则（辅助线索，永不改判 actor）。
#[derive(Debug, Clone)]
pub struct ActorRules {
    /// 命中即 channel=browseros 的窗口标题尾缀（大小写不敏感）。
    pub agent_title_suffixes: Vec<String>,
}

impl Default for ActorRules {
    fn default() -> Self {
        ActorRules {
            agent_title_suffixes: vec![" - BrowserOS".to_string()],
        }
    }
}

impl ActorRules {
    /// 窗口标题是否命中 AI 通道规则（尾缀匹配，大小写不敏感）。
    pub fn is_agent_channel_title(&self, title: &str) -> bool {
        let lower = title.to_lowercase();
        self.agent_title_suffixes
            .iter()
            .any(|s| !s.is_empty() && lower.ends_with(&s.to_lowercase()))
    }
}

/// 单分钟证据（epoch 分钟 → (E_h, E_a)）。SUM 与逐行 MAX 对布尔判定可证等价
/// （各项均非负钳零），与 presence 权威 SQL 同口径。
type EvidenceMap = HashMap<i64, (bool, bool)>;

/// 从 events 现算窗口内分钟证据（epoch 分钟空间）。单次 GROUP BY 扫描，
/// 行级表达式与 agg_minute_actor 重建路径逐字一致（同一组宏）。
fn evidence_from_events(conn: &Connection, start: &str, end: &str) -> EvidenceMap {
    let sql = format!(
        "SELECT CAST(strftime('%s', timestamp) AS INTEGER) / 60 AS me, \
                SUM({keys_row}) AS kt, SUM({inj_keys_row}) AS ki, \
                SUM({clicks_row}) AS ct, SUM({inj_clicks_row}) AS ci, \
                SUM({scroll_row}) AS st, SUM({inj_scroll_row}) AS si \
         FROM events \
         WHERE timestamp >= ?1 AND timestamp < ?2 AND event_type IN ('keyboard','mouse') \
         GROUP BY me",
        keys_row = KEYS_ROW_EXPR,
        inj_keys_row = INJECTED_KEYS_ROW_EXPR,
        clicks_row = CLICKS_ROW_EXPR,
        inj_clicks_row = INJECTED_CLICKS_ROW_EXPR,
        scroll_row = SCROLL_ROW_EXPR,
        inj_scroll_row = INJECTED_SCROLL_ROW_EXPR,
    );
    let mut map = EvidenceMap::new();
    let Ok(mut stmt) = conn.prepare(&sql) else {
        return map;
    };
    let Ok(rows) = stmt.query_map(params![start, end], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, i64>(2)?,
            r.get::<_, i64>(3)?,
            r.get::<_, i64>(4)?,
            r.get::<_, i64>(5)?,
            r.get::<_, i64>(6)?,
        ))
    }) else {
        return map;
    };
    for row in rows.flatten() {
        let (me, kt, ki, ct, ci, st, si) = row;
        // 勘误 1：E_a = injected_keys + injected_clicks + injected_scroll_ticks > 0
        let injected = ki + ci + si > 0;
        // E_h：各项 MAX(总-注入, 0) 求和 > 0（负值钳零，脏数据不得制造真人证据）
        let human = (kt - ki).max(0) + (ct - ci).max(0) + (st - si).max(0) > 0;
        map.insert(me, (human, injected));
    }
    map
}

/// 前台区间（epoch 秒）：app + 窗口标题 + [start, end)。
struct FgInterval<'a> {
    app: &'a str,
    title: &'a str,
    start: i64,
    end: i64,
}

/// 单应用 actor 驻留估算（勘误 4：全部为按前台覆盖秒数的估算分摊）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EstimatedActorDwellSec {
    pub user: i64,
    pub agent: i64,
    pub unattended: i64,
    pub mixed: i64,
    pub unknown: i64,
}

impl EstimatedActorDwellSec {
    /// agent 类的证据类型（勘误 2：注入输入代理指标，不宣称一定是 AI Agent）。
    pub const AGENT_EVIDENCE_TYPE: &'static str = "injected_input";

    pub fn total(&self) -> i64 {
        self.user + self.agent + self.unattended + self.mixed + self.unknown
    }

    fn add(&mut self, actor: Actor, secs: i64) {
        match actor {
            Actor::User => self.user += secs,
            Actor::Agent => self.agent += secs,
            Actor::Unattended => self.unattended += secs,
            Actor::Mixed => self.mixed += secs,
            Actor::Unknown => self.unknown += secs,
        }
    }
}

/// 单应用的 actor 驻留估算行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppActorDwell {
    pub app: String,
    pub estimated_actor_dwell_sec: EstimatedActorDwellSec,
    /// channel=browseros 的覆盖秒数（独立维度，可与五类任一叠加；辅助线索）。
    pub browseros_sec: i64,
}

/// actor 驻留口径说明（供 API/MCP/CLI 响应显式自述口径——估算值，非精确值）。
pub const FG_ACTOR_DWELL_METHOD: &str = "按前台覆盖秒数分摊的 actor 估算（estimated），agent=注入输入代理指标（injected_input），未归属秒数单独呈现";

/// 主题口径说明（来源显式标注；当前唯一可用来源是窗口标题聚类）。
pub const TOPIC_METHOD: &str = "主题 = 窗口标题去浏览器尾缀聚类（source=title）；域名来源待事件携带可验证 URL 字段后启用；默认输出脱敏键，敏感标题永不输出明文";

/// 窗口 [start, end)（UTC RFC3339）内各前台应用的五类 actor 驻留估算。
///
/// 口径与 [`super::foreground_dwell_by_app`] 同构（同一 switch 区间重建、同一
/// 停摆封顶、同一 credit_tail 语义），但**不改变**该函数的返回——0.3.2 既有
/// 口径保持原样（契约边界锁死）。返回 (按总秒数降序的应用行, 未归属秒数)：
/// 未归属秒数 = 无前台覆盖的分钟内的秒（unknown，CLI/API 必须可见）。
pub fn foreground_actor_dwell(
    conn: &Connection,
    start: &str,
    end: &str,
    stall_cap_secs: i64,
    credit_tail: bool,
    rules: &ActorRules,
) -> (Vec<AppActorDwell>, i64) {
    let evidence = evidence_from_events(conn, start, end);
    let parse = |t: &str| {
        chrono::DateTime::parse_from_rfc3339(t)
            .ok()
            .map(|t| t.timestamp())
    };
    let (Some(start_t), Some(end_t)) = (parse(start), parse(end)) else {
        return (Vec::new(), 0);
    };

    // switch 区间重建（与 foreground_dwell_by_app 同一查询谓词；标题额外取回
    // 供 channel 判定——window 行的标题在 window_title 列与 event_data.title）。
    let Ok(mut stmt) = conn.prepare(
        "SELECT timestamp, \
                COALESCE(NULLIF(app_name,''), '(unknown)'), \
                COALESCE(NULLIF(window_title,''), '') \
         FROM events \
         WHERE event_type = 'window' AND event_action = 'switch' \
           AND timestamp >= ?1 AND timestamp < ?2 ORDER BY timestamp",
    ) else {
        return (Vec::new(), 0);
    };
    let rows: Vec<(String, String, String)> = stmt
        .query_map(params![start, end], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })
        .map(|v| v.flatten().collect())
        .unwrap_or_default();
    let switches: Vec<(i64, String, String)> = rows
        .iter()
        .filter_map(|(ts, app, title)| parse(ts).map(|t| (t, app.clone(), title.clone())))
        .collect();
    if switches.is_empty() {
        // 无任何前台证据：整窗未归属（unknown），如实返回（窗口总秒）
        let window_secs = (end_t - start_t).max(0);
        return (Vec::new(), window_secs);
    }

    let mut intervals: Vec<FgInterval> = Vec::new();
    for pair in switches.windows(2) {
        let secs = (pair[1].0 - pair[0].0).max(0).min(stall_cap_secs);
        if secs > 0 {
            intervals.push(FgInterval {
                app: &pair[0].1,
                title: &pair[0].2,
                start: pair[0].0,
                end: pair[0].0 + secs,
            });
        }
    }
    if credit_tail {
        if let Some(last) = switches.last() {
            let secs = (end_t - last.0).max(0).min(stall_cap_secs);
            if secs > 0 {
                intervals.push(FgInterval {
                    app: &last.1,
                    title: &last.2,
                    start: last.0,
                    end: last.0 + secs,
                });
            }
        }
    }

    let mut out: HashMap<String, AppActorDwell> = HashMap::new();
    let mut covered_secs_total = 0i64;
    for it in &intervals {
        let entry = out
            .entry(it.app.to_string())
            .or_insert_with(|| AppActorDwell {
                app: it.app.to_string(),
                estimated_actor_dwell_sec: EstimatedActorDwellSec::default(),
                browseros_sec: 0,
            });
        let is_channel = rules.is_agent_channel_title(it.title);
        // 与分钟交叉：按重叠秒数分摊（区间互不重叠 → 每分钟总分配 ≤60s）
        let first_minute = it.start.div_euclid(60);
        let last_minute = (it.end - 1).div_euclid(60);
        for m in first_minute..=last_minute {
            let m_start = m * 60;
            let overlap = it.end.min(m_start + 60) - it.start.max(m_start);
            if overlap <= 0 {
                continue;
            }
            let (human, injected) = evidence.get(&m).copied().unwrap_or((false, false));
            let actor = Actor::from_evidence(human, injected, true);
            entry.estimated_actor_dwell_sec.add(actor, overlap);
            if is_channel {
                entry.browseros_sec += overlap;
            }
            covered_secs_total += overlap;
        }
    }

    // 未归属秒数：窗口总秒 - 已归属前台覆盖秒。停摆封顶截断的部分天然归
    // unknown（采集空洞不是任何应用的前台覆盖）。
    let window_secs = (end_t - start_t).max(0);
    let uncovered = (window_secs - covered_secs_total).max(0);

    let mut rows: Vec<AppActorDwell> = out.into_values().collect();
    rows.sort_by(|a, b| {
        let ta = a.estimated_actor_dwell_sec.total();
        let tb = b.estimated_actor_dwell_sec.total();
        tb.cmp(&ta).then_with(|| a.app.cmp(&b.app))
    });
    (rows, uncovered)
}

/// ai_suspect 合成判定（推测指标）：agent（注入输入证据）或 AI 通道上的
/// unattended（BrowserOS 前台无输入——典型的 AI 自主浏览形态）。
/// 输出方必须随附「推测」标注，不得冒称事实判定（勘误 2）。
pub fn is_ai_suspect(actor: Actor, agent_channel: bool) -> bool {
    actor == Actor::Agent || (agent_channel && actor == Actor::Unattended)
}

// ─── F2：主题聚合（查询层，无新缓存）─────────────────────────────────────────

/// 敏感标题模式（子串、大小写不敏感）：命中即聚合为 <redacted:sensitive>，
/// 明文不入任何输出（--raw 读原始路径时也再过一遍，双保险）。
const SENSITIVE_TITLE_PATTERNS: &[&str] = &[
    "login",
    "signin",
    "sign-in",
    "token",
    "credential",
    "password",
    "passwd",
    "secret",
    "登录",
    "登陆",
    "密码",
    "凭证",
    "凭据",
    "验证码",
];

pub const REDACTED_TOPIC_KEY: &str = "<redacted:sensitive>";

/// 主题来源标签（GPT 契约：来源必须显式标注，不冒称）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TopicSource {
    /// 窗口标题去浏览器尾缀聚类（当前唯一可用来源）
    Title,
    /// 域名——仅当事件携带可验证 URL 字段时才可能产出；当前 window/browser
    /// 事件只落 title 不落 URL，此来源恒为空集（如实声明，不伪造）。
    Domain,
    /// 用户显式配置规则（预留，0.4.0 无编辑层）
    Rule,
}

impl TopicSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            TopicSource::Title => "title",
            TopicSource::Domain => "domain",
            TopicSource::Rule => "rule",
        }
    }
}

/// 主题聚合行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TopicSummary {
    /// 主题键（默认脱敏后；--raw 且非敏感时为明文标题）。
    pub topic: String,
    pub source: TopicSource,
    /// 命中敏感模式——明文永不输出（含 --raw），topic 恒为 REDACTED_TOPIC_KEY。
    pub sensitive: bool,
    pub event_count: i64,
    /// 该主题前台覆盖的 actor 驻留估算（同应用级口径，按秒分摊）。
    pub estimated_actor_dwell_sec: EstimatedActorDwellSec,
    pub browseros_sec: i64,
}

/// 标题 → 主题键归一化：剥 AI 通道与常见浏览器尾缀 → 敏感检测 → 空标题剔除。
/// 返回 None 表示无可聚合主题（空标题）。
pub fn normalize_topic(
    raw_title: &str,
    rules: &ActorRules,
    keep_raw: bool,
) -> Option<(String, bool)> {
    let mut t = raw_title.trim().to_string();
    if t.is_empty() {
        return None;
    }
    // 剥尾缀：AI 通道规则 + 常见浏览器窗尾（剥命中的最长尾缀，避免过度剥除）
    let mut suffixes: Vec<&str> = rules
        .agent_title_suffixes
        .iter()
        .map(|s| s.as_str())
        .collect();
    suffixes.extend_from_slice(&[
        " - Google Chrome",
        " - Microsoft Edge",
        " - Mozilla Firefox",
        " - Brave",
        " - Firefox",
        " - Chromium",
    ]);
    let lower = t.to_lowercase();
    let mut best: Option<&str> = None;
    for s in &suffixes {
        if !s.is_empty() && lower.ends_with(&s.to_lowercase()) {
            match best {
                Some(b) if b.len() >= s.len() => {}
                _ => best = Some(s),
            }
        }
    }
    if let Some(s) = best {
        let cut = t.len() - s.len();
        t = t[..cut].trim_end().to_string();
    }
    if t.is_empty() {
        return None;
    }
    // 敏感检测（输出侧双保险之一；--raw 也过）
    let lower = t.to_lowercase();
    let sensitive = SENSITIVE_TITLE_PATTERNS
        .iter()
        .any(|p| lower.contains(&p.to_lowercase()));
    if sensitive {
        return Some((REDACTED_TOPIC_KEY.to_string(), true));
    }
    if !keep_raw {
        // 默认脱敏：只保留首个分隔符前的主标题（如「页面名 - 站点名」→「页面名」），
        // 降低明文标题进入报告的暴露面；--raw 走显式原始路径才保留全标题。
        if let Some(pos) = t.find(" - ") {
            let head = t[..pos].trim();
            if !head.is_empty() {
                t = head.to_string();
            }
        }
    }
    Some((t, false))
}

/// 窗口 [start, end) 内的主题聚合（应用/主题两级中的主题级）。
///
/// 无主题缓存（GPT 契约：派生缓存只存输入证据；标题明文不入任何缓存）。
/// 主题驻留 = 前台区间按其窗口标题主题归并后秒数分摊（估算口径同应用级）。
/// `keep_raw=false`（默认）输出脱敏键；`true` 走显式原始读取路径且仍过敏感
/// 词表——绝不从脱敏键假装恢复明文（--raw 契约）。
pub fn topics_summary(
    conn: &Connection,
    start: &str,
    end: &str,
    stall_cap_secs: i64,
    credit_tail: bool,
    rules: &ActorRules,
    keep_raw: bool,
) -> Vec<TopicSummary> {
    let evidence = evidence_from_events(conn, start, end);
    let parse = |t: &str| {
        chrono::DateTime::parse_from_rfc3339(t)
            .ok()
            .map(|t| t.timestamp())
    };
    let Some(end_t) = parse(end) else {
        return Vec::new();
    };
    let Ok(mut stmt) = conn.prepare(
        "SELECT timestamp, \
                COALESCE(NULLIF(window_title,''), '') \
         FROM events \
         WHERE event_type = 'window' AND event_action = 'switch' \
           AND timestamp >= ?1 AND timestamp < ?2 ORDER BY timestamp",
    ) else {
        return Vec::new();
    };
    let rows: Vec<(String, String)> = stmt
        .query_map(params![start, end], |r| Ok((r.get(0)?, r.get(1)?)))
        .map(|v| v.flatten().collect())
        .unwrap_or_default();
    let switches: Vec<(i64, String)> = rows
        .iter()
        .filter_map(|(ts, title)| parse(ts).map(|t| (t, title.clone())))
        .collect();

    // 区间 → 主题（区间起点窗口的主题；空主题归 (untitled)）
    let mut intervals: Vec<(i64, i64, String, bool)> = Vec::new();
    for pair in switches.windows(2) {
        let secs = (pair[1].0 - pair[0].0).max(0).min(stall_cap_secs);
        if secs > 0 {
            let (key, _sensitive) = normalize_topic(&pair[0].1, rules, keep_raw)
                .unwrap_or(("(untitled)".to_string(), false));
            let channel = rules.is_agent_channel_title(&pair[0].1);
            intervals.push((pair[0].0, pair[0].0 + secs, key, channel));
        }
    }
    if credit_tail {
        if let Some(last) = switches.last() {
            let secs = (end_t - last.0).max(0).min(stall_cap_secs);
            if secs > 0 {
                let (key, _sensitive) = normalize_topic(&last.1, rules, keep_raw)
                    .unwrap_or(("(untitled)".to_string(), false));
                let channel = rules.is_agent_channel_title(&last.1);
                intervals.push((last.0, last.0 + secs, key, channel));
            }
        }
    }

    #[derive(Default, Clone)]
    struct Acc {
        count: i64,
        dwell: EstimatedActorDwellSec,
        browseros: i64,
    }
    let mut acc: HashMap<String, Acc> = HashMap::new();
    // 事件计数：每个前台区间计一次主题命中
    for (_, _, key, _) in &intervals {
        acc.entry(key.clone()).or_default().count += 1;
    }
    for it in &intervals {
        let first_minute = it.0.div_euclid(60);
        let last_minute = (it.1 - 1).div_euclid(60);
        for m in first_minute..=last_minute {
            let m_start = m * 60;
            let overlap = it.1.min(m_start + 60) - it.0.max(m_start);
            if overlap <= 0 {
                continue;
            }
            let (human, injected) = evidence.get(&m).copied().unwrap_or((false, false));
            let actor = Actor::from_evidence(human, injected, true);
            let a = acc.entry(it.2.clone()).or_default();
            a.dwell.add(actor, overlap);
            if it.3 {
                a.browseros += overlap;
            }
        }
    }

    let mut rows: Vec<TopicSummary> = acc
        .into_iter()
        .map(|(topic, a)| TopicSummary {
            sensitive: topic == REDACTED_TOPIC_KEY,
            topic,
            source: TopicSource::Title,
            event_count: a.count,
            estimated_actor_dwell_sec: a.dwell,
            browseros_sec: a.browseros,
        })
        .collect();
    rows.sort_by(|a, b| {
        let ta = a.estimated_actor_dwell_sec.total();
        let tb = b.estimated_actor_dwell_sec.total();
        tb.cmp(&ta)
            .then_with(|| b.event_count.cmp(&a.event_count))
            .then_with(|| a.topic.cmp(&b.topic))
    });
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Event, EventAction, EventType};
    use rusqlite::params;

    fn setup_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        // 全迁移链（SCHEMA 只是 0001；agg_minute_actor 由 0011 建）
        crate::db::schema::run_migrations(&conn).unwrap();
        conn
    }

    fn ins_switch(conn: &Connection, ts: &str, app: &str, title: &str) {
        conn.execute(
            "INSERT INTO events (timestamp, event_type, event_action, app_name, window_title) \
             VALUES (?1, 'window', 'switch', ?2, ?3)",
            params![ts, app, title],
        )
        .unwrap();
    }

    fn ins_input_agg(conn: &Connection, ts: &str, etype: &str, data: &str) {
        conn.execute(
            "INSERT INTO events (timestamp, event_type, event_action, event_data) \
             VALUES (?1, ?2, 'input_agg', ?3)",
            params![ts, etype, data],
        )
        .unwrap();
    }

    /// 真值表完备性：五类互斥且每个 (human, injected, covered) 组合都有判定。
    #[test]
    fn actor_truth_table_is_complete_and_mutually_exclusive() {
        let cases = [
            (true, false, true, Actor::User),
            (false, true, true, Actor::Agent),
            (true, true, true, Actor::Mixed),
            (false, false, true, Actor::Unattended),
            (true, false, false, Actor::Unknown),
            (false, true, false, Actor::Unknown),
            (true, true, false, Actor::Unknown),
            (false, false, false, Actor::Unknown),
        ];
        for (h, i, c, expect) in cases {
            assert_eq!(Actor::from_evidence(h, i, c), expect, "h={h} i={i} c={c}");
        }
        let all = [
            Actor::User,
            Actor::Agent,
            Actor::Unattended,
            Actor::Mixed,
            Actor::Unknown,
        ];
        assert_eq!(all.len(), 5, "五类完备（GPT 契约）");
    }

    /// 真人点击不得判 agent（勘误 1 阻断级）：E_a 只认注入列之和。
    #[test]
    fn human_clicks_are_never_agent() {
        let conn = setup_conn();
        ins_input_agg(
            &conn,
            "2026-10-24T10:00:30Z",
            "mouse",
            r#"{"clicks":5,"injected_clicks":0,"scroll_ticks":0}"#,
        );
        let ev = evidence_from_events(&conn, "2026-10-24T00:00:00Z", "2026-10-25T00:00:00Z");
        assert_eq!(ev.len(), 1, "应有该分钟证据");
        let (human, injected) = *ev.values().next().unwrap();
        assert!(!injected, "真人点击不得产生注入证据（勘误 1 阻断级）");
        assert!(human);
        assert_eq!(Actor::from_evidence(human, injected, true), Actor::User);
    }

    /// 纯注入分钟 → agent；混合分钟 → mixed。
    #[test]
    fn injected_only_is_agent_and_mix_is_mixed() {
        let conn = setup_conn();
        ins_input_agg(
            &conn,
            "2026-10-24T10:00:30Z",
            "keyboard",
            r#"{"keys":500,"injected_keys":500,"samples":500}"#,
        );
        ins_input_agg(
            &conn,
            "2026-10-24T10:01:30Z",
            "keyboard",
            r#"{"keys":10,"injected_keys":2,"samples":10}"#,
        );
        let ev = evidence_from_events(&conn, "2026-10-24T00:00:00Z", "2026-10-25T00:00:00Z");
        let mut mins: Vec<(i64, Actor)> = ev
            .iter()
            .map(|(k, (h, i))| (*k, Actor::from_evidence(*h, *i, true)))
            .collect();
        mins.sort_by_key(|(k, _)| *k);
        assert_eq!(mins[0].1, Actor::Agent, "纯注入分钟 → agent");
        assert_eq!(mins[1].1, Actor::Mixed, "真人+注入并存 → mixed");
    }

    /// 无覆盖分钟 → unknown（绝不硬塞）；归属+未归属守恒（勘误 4）。
    #[test]
    fn uncovered_is_unknown_and_allocation_conserved() {
        let conn = setup_conn();
        ins_switch(&conn, "2026-10-24T10:00:00Z", "appA", "Doc - Editor");
        ins_input_agg(
            &conn,
            "2026-10-24T10:00:30Z",
            "keyboard",
            r#"{"keys":30,"samples":30}"#,
        );
        // credit_tail=false：单 switch 无区间 → 覆盖 0 秒；窗口 12h 全部未归属
        let (rows, uncovered) = foreground_actor_dwell(
            &conn,
            "2026-10-24T00:00:00Z",
            "2026-10-24T12:00:00Z",
            7200,
            false,
            &ActorRules::default(),
        );
        assert!(rows.is_empty(), "单 switch 无区间（credit_tail=false）");
        assert_eq!(uncovered, 12 * 3600, "整窗未归属 = unknown 秒数如实呈现");
        // credit_tail=true：末 switch 记到窗口上界（≤封顶 7200s）
        let (rows, uncovered2) = foreground_actor_dwell(
            &conn,
            "2026-10-24T00:00:00Z",
            "2026-10-24T12:00:00Z",
            7200,
            true,
            &ActorRules::default(),
        );
        let a = rows.iter().find(|r| r.app == "appA").expect("appA 应有行");
        assert_eq!(
            a.estimated_actor_dwell_sec.total() + uncovered2,
            12 * 3600,
            "归属+未归属 = 窗口总秒（守恒）"
        );
        // 分摊：10:00 分钟（有真人输入）→ user 60s；10:01-12:00 无输入 →
        // unattended 7140s；每分钟归属恰 60s（≤60 契约）
        assert_eq!(a.estimated_actor_dwell_sec.user, 60);
        assert_eq!(a.estimated_actor_dwell_sec.unattended, 7140);
        assert_eq!(a.estimated_actor_dwell_sec.total(), 7200);
        assert_eq!(uncovered2, 12 * 3600 - 7200);
    }

    /// 停摆封顶：>cap 的间隔按 cap 归属，超出部分归 unknown（GPT 边界要求）。
    #[test]
    fn stall_gap_beyond_cap_is_not_attributed() {
        let conn = setup_conn();
        ins_switch(&conn, "2026-10-24T08:00:00Z", "appA", "Doc");
        ins_switch(&conn, "2026-10-24T14:00:00Z", "appB", "Page");
        ins_input_agg(
            &conn,
            "2026-10-24T08:00:30Z",
            "keyboard",
            r#"{"keys":10,"samples":10}"#,
        );
        // 6h 间隔 > 2h 封顶：appA 只得 7200s，其余 4h 未归属
        let (rows, uncovered) = foreground_actor_dwell(
            &conn,
            "2026-10-24T00:00:00Z",
            "2026-10-25T00:00:00Z",
            7200,
            false,
            &ActorRules::default(),
        );
        let a = rows.iter().find(|r| r.app == "appA").expect("appA 应有行");
        assert_eq!(a.estimated_actor_dwell_sec.total(), 7200, "封顶归属");
        // 窗口 24h：appA 区间封顶 7200s，其余 79200s 无前台覆盖 → 未归属
        assert_eq!(uncovered, 86400 - 7200);
    }

    /// 同分钟多次快照 UPSERT + 跨会话重启（GPT 点名测试）：MAX 合并不重复累计。
    #[test]
    fn actor_cache_snapshot_upsert_is_idempotent_across_restart() {
        let conn = setup_conn();
        let mk = |data: &str| Event {
            timestamp: "2026-10-24T10:00:30+00:00".to_string(),
            event_type: EventType::Keyboard,
            event_action: EventAction::InputAgg,
            event_data: Some(serde_json::from_str(data).unwrap()),
            app_name: None,
            window_title: None,
            session_id: None,
        };
        // 会话 1：写入 keys=30 快照
        crate::db::agg::apply_actor_event_for_test(
            &conn,
            &mk(r#"{"keys":30,"samples":30,"final":false}"#),
            1,
        )
        .unwrap();
        // 会话 2（重启后）：同一分钟新会话累计 keys=2 —— 快照 MAX 合并
        crate::db::agg::apply_actor_event_for_test(
            &conn,
            &mk(r#"{"keys":2,"samples":2,"final":false}"#),
            2,
        )
        .unwrap();
        let n: i64 = conn
            .query_row("SELECT keys_total FROM agg_minute_actor", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 30, "快照 MAX 合并：max(30,2)=30，不累加不回退");
        // 重复投递同一行（rowid 相同）也不变
        crate::db::agg::apply_actor_event_for_test(
            &conn,
            &mk(r#"{"keys":30,"samples":30,"final":false}"#),
            1,
        )
        .unwrap();
        let n2: i64 = conn
            .query_row("SELECT keys_total FROM agg_minute_actor", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n2, 30);
    }

    /// 重建/增量逐行等价（GPT 必过清单）：混合形态分钟双路径结果一致。
    #[test]
    fn rebuild_and_incremental_actor_cache_are_row_equal() {
        let conn = setup_conn();
        // 混合形态：raw press（含注入）+ raw click + input_agg 行（点击+滚轮）
        conn.execute(
            "INSERT INTO events (timestamp, event_type, event_action, event_data) \
             VALUES ('2026-10-24T10:00:10Z', 'keyboard', 'press', '{\"injected\": false}')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO events (timestamp, event_type, event_action, event_data) \
             VALUES ('2026-10-24T10:00:20Z', 'keyboard', 'press', '{\"injected\": true}')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO events (timestamp, event_type, event_action, event_data) \
             VALUES ('2026-10-24T10:00:30Z', 'mouse', 'click', '{}')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO events (timestamp, event_type, event_action, event_data) \
             VALUES ('2026-10-24T10:01:00Z', 'mouse', 'input_agg', \
                     '{\"clicks\":3,\"scroll_ticks\":7,\"injected_clicks\":1,\"injected_scroll_ticks\":2,\"samples\":4}')",
            [],
        )
        .unwrap();

        // 路径 A：全量重建（rebuild_actor_cache）
        crate::db::agg::rebuild_actor_cache(&conn).unwrap();
        let mut rebuilt: Vec<(i64, i64, i64, i64, i64, i64, i64)> = conn
            .prepare("SELECT hour*60+minute, keys_total, keys_injected, clicks_total, clicks_injected, scroll_total, scroll_injected FROM agg_minute_actor ORDER BY 1")
            .unwrap()
            .query_map([], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                ))
            })
            .map(|v| v.flatten().collect())
            .unwrap();

        // 路径 B：清空后按事件逐条增量（apply_actor_event）
        conn.execute("DELETE FROM agg_minute_actor", []).unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT id, timestamp, event_type, event_action, event_data FROM events \
                 WHERE event_type IN ('keyboard','mouse') ORDER BY id",
            )
            .unwrap();
        let evs: Vec<(i64, String, String, String, Option<String>)> = stmt
            .query_map([], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })
            .map(|v| v.flatten().collect())
            .unwrap();
        drop(stmt);
        let parse_type = |s: &str| match s {
            "keyboard" => EventType::Keyboard,
            "mouse" => EventType::Mouse,
            other => panic!("测试未覆盖事件类型: {other}"),
        };
        let parse_action = |s: &str| match s {
            "press" => EventAction::Press,
            "click" => EventAction::Click,
            "input_agg" => EventAction::InputAgg,
            other => panic!("测试未覆盖事件动作: {other}"),
        };
        for (id, ts, ty, ac, data) in evs {
            let e = Event {
                timestamp: ts,
                event_type: parse_type(&ty),
                event_action: parse_action(&ac),
                event_data: data.as_deref().map(|d| serde_json::from_str(d).unwrap()),
                app_name: None,
                window_title: None,
                session_id: None,
            };
            crate::db::agg::apply_actor_event_for_test(&conn, &e, id).unwrap();
        }
        let mut incremental: Vec<(i64, i64, i64, i64, i64, i64, i64)> = conn
            .prepare("SELECT hour*60+minute, keys_total, keys_injected, clicks_total, clicks_injected, scroll_total, scroll_injected FROM agg_minute_actor ORDER BY 1")
            .unwrap()
            .query_map([], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                ))
            })
            .map(|v| v.flatten().collect())
            .unwrap();

        assert_eq!(rebuilt.len(), incremental.len(), "行数一致");
        rebuilt.sort();
        incremental.sort();
        assert_eq!(rebuilt, incremental, "GPT 契约：增量/重建逐行等价");
    }

    /// 敏感标题不进普通主题输出（GPT 必过清单）；--raw 也不恢复明文。
    #[test]
    fn sensitive_titles_never_leak_into_topics() {
        let rules = ActorRules::default();
        let (key, sensitive) =
            normalize_topic("登录 - Example Dashboard - BrowserOS", &rules, false).unwrap();
        assert!(sensitive);
        assert_eq!(key, REDACTED_TOPIC_KEY);
        // --raw 路径同样拦截
        let (key2, sensitive2) = normalize_topic("login page", &rules, true).unwrap();
        assert!(sensitive2);
        assert_eq!(key2, REDACTED_TOPIC_KEY);
        // 非敏感标题默认剥主标题后的部分
        let (key3, s3) = normalize_topic("日报草稿 - 内部系统 - BrowserOS", &rules, false).unwrap();
        assert!(!s3);
        assert_eq!(key3, "日报草稿");
        // --raw 保留全标题（剥浏览器尾缀后）
        let (key4, _) = normalize_topic("日报草稿 - 内部系统 - BrowserOS", &rules, true).unwrap();
        assert_eq!(key4, "日报草稿 - 内部系统");
    }

    /// ai_suspect 合成口径（勘误 2）：仅注入证据或 BrowserOS 通道上的 unattended。
    #[test]
    fn ai_suspect_composition() {
        assert!(is_ai_suspect(Actor::Agent, false));
        assert!(is_ai_suspect(Actor::Unattended, true));
        // 契约公式：ai_suspect = agent ∪ (browseros ∧ unattended)——Mixed 不在
        // 集合内（其注入证据已由自身 actor 类承载，合成指标不扩面）
        assert!(!is_ai_suspect(Actor::Mixed, true));
        assert!(
            !is_ai_suspect(Actor::User, true),
            "真人在 BrowserOS 操作仍是 user（线索不改判）"
        );
        assert!(!is_ai_suspect(Actor::Unattended, false));
        assert!(!is_ai_suspect(Actor::Unknown, false));
    }

    /// BrowserOS 通道识别：尾缀命中（大小写不敏感）、非尾缀不误报。
    #[test]
    fn browseros_channel_rule() {
        let rules = ActorRules::default();
        assert!(rules.is_agent_channel_title("DeepSeek - 探索未至之境 - BrowserOS"));
        assert!(rules.is_agent_channel_title("docs - browseros"));
        assert!(!rules.is_agent_channel_title("BrowserOS 官网下载"));
        assert!(!rules.is_agent_channel_title("Doc - Editor"));
    }

    /// 历史回算不改 events（GPT 必过清单）：rebuild_actor_cache 前后 events 表
    /// 内容逐行一致（只读派生，原始层永不改动）。
    #[test]
    fn recompute_actor_cache_never_touches_events() {
        let conn = setup_conn();
        ins_switch(&conn, "2026-10-24T10:00:00Z", "appA", "Doc");
        ins_input_agg(
            &conn,
            "2026-10-24T10:00:30Z",
            "keyboard",
            r#"{"keys":30,"samples":30}"#,
        );
        let before: Vec<(i64, String, String, String)> = conn
            .prepare("SELECT id, timestamp, event_type, event_action FROM events ORDER BY id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .map(|v| v.flatten().collect())
            .unwrap();
        crate::db::agg::rebuild_actor_cache(&conn).unwrap();
        let after: Vec<(i64, String, String, String)> = conn
            .prepare("SELECT id, timestamp, event_type, event_action FROM events ORDER BY id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .map(|v| v.flatten().collect())
            .unwrap();
        assert_eq!(before, after, "历史回算绝不改写 events（GPT 验收门禁）");
        // 缓存确实产出
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM agg_minute_actor", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
    }
}
