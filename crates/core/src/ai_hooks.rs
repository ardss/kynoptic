//! F05 真实事件通道（PRD v1.0 Stage 2）——AI 工具 hook 事件摄取。
//!
//! 文件扫描（[`crate::ai_adapter`]）是 inferred 后备；本模块是 **observed
//! 真实通道**：AI 工具的 hook 在工具运行时实时把事件推给 kynoptic，证据
//! 等级 observed（事件发生那一刻我们亲眼收到）。
//!
//! ## 三种工具的 hook 机制调研结论（2026-11-03，依据本机真实文档/目录）
//!
//! ### claude_code —— 有 hooks，全接入
//! 依据：Claude Code 官方 hooks 文档口径（settings.json `hooks` 键）。
//! 支持事件：PreToolUse / PostToolUse / Notification / Stop（另有
//! SessionStart / UserPromptSubmit，同样接）。stdin JSON 含 `session_id`、
//! `hook_event_name`、`tool_name`（工具事件）等；**payload 无时间戳字段**
//! ——以 kynoptic 收到时刻为 observed 事件时间（收到即观察到，不虚构
//! 工具内部时刻）。
//!
//! ### zcode —— 有 hooks，接入 7 事件
//! 依据：本机 `~/.zcode/cli/plugins/cache/zcode-plugins-official/
//! zcode-guide/0.3.0/skills/diagnosing-hooks/SKILL.md`（ZCode 官方
//! hook 诊断文档）。事件词表**恰好七个**：SessionStart / UserPromptSubmit /
//! PreToolUse / PermissionRequest / PostToolUse / PostToolUseFailure / Stop
//! ——**Notification 不支持**（文档明示 unsupported，不虚构）。配置在
//! `~/.zcode/cli/config.json` 顶层 `hooks` 键，必须 `enabled: true`（配置
//! 文件 hooks 默认禁用）。`process` 型 hook（command + args，无 shell，
//! Windows 安全）；会话标识模板变量 `${CLAUDE_SESSION_ID}` 由 ZCode 展开。
//!
//! ### dsh —— 无 hook/插件事件机制，如实不接入
//! 调研：`K:/OBSIDIAN/AI/.dsh/`（本机真实目录）只有 workflow-artifacts /
//! workflows / workflow-drafts 产物目录，无 hooks.json / hooks 配置面；
//! DSH 运行生命周期只在其内部 journal 里，未暴露用户可配置的事件钩子。
//! **该源保留 ai_adapter 文件扫描（inferred）后备**，不虚构 hook 通道。
//!
//! ## 写库口径（铁律）
//! - 事件写进现有 events 表（不建第二真相表）：`event_type='ai_session'`、
//!   `event_action`=动作词表值、`quality='observed'`；
//! - 幂等（复审 2026-11-11 三档）：`ext_event_id = "{source}:{键}"`（migration
//!   0017 部分唯一索引 + INSERT OR IGNORE）。payload 显式带字符串 `event_id`
//!   （去重头）→ 键即该 id，同一条重试重发不重复写；payload 自带 `timestamp`
//!   → 键 = payload_fnv64（重发字节相同 → 去重）；payload 无 timestamp（实测
//!   常态，ts=接收时刻）→ 键 = fnv64(payload 字节 + 接收毫秒)——两次内容相同
//!   的接收是两次独立 observed 发生，不再被误判成重发吞掉（复审 high：纯
//!   payload 哈希无法区分「同一条事件的重发」与「第二次发生的相同事件」）；
//! - `source_id` / `app_name` = 工具源名（claude_code / zcode）；
//! - interval 派生在 [`crate::db::activity_intervals`]：hook observed 优先、
//!   扫描 inferred 兜底、同 (source, session_id) 不双计、同源重叠区间并集。
//!
//! 校验风格与 dash `api_ext_events_post_at` 的编辑器分支一致：整批先验后写，
//! 任一条失败整批拒收。

use rusqlite::{params, Connection};
use serde_json::Value;

/// 有真实 hook 机制的源词表（dsh 无 hook 机制，不在此列——如实缺省）。
pub const AI_HOOK_SOURCES: &[&str] = &["claude_code", "zcode"];

/// 动作词表（hook 事件名 → 规范动作，两源共用一张映射）。
pub const AI_HOOK_ACTIONS: &[&str] = &[
    "session_start",
    "prompt_submit",
    "tool_call",
    "permission_request",
    "tool_result",
    "tool_error",
    "notification",
    "session_stop",
];

/// hook 事件名 → 规范动作。两源的英文事件名共用大小写不敏感匹配
/// （claude_code 与 zcode 事件名一致口径：PreToolUse 等）。
fn map_hook_event(name: &str) -> Option<&'static str> {
    match name.to_ascii_lowercase().as_str() {
        "sessionstart" => Some("session_start"),
        "userpromptsubmit" => Some("prompt_submit"),
        "pretooluse" => Some("tool_call"),
        "permissionrequest" => Some("permission_request"),
        "posttooluse" => Some("tool_result"),
        "posttoolusefailure" => Some("tool_error"),
        "notification" => Some("notification"),
        "stop" => Some("session_stop"),
        _ => None,
    }
}

/// FNV-1a 64-bit（与 activity_intervals::title_hash 同显式算法；跨版本稳定
/// ——ext_event_id 持久化在库里，不能用 rustc 会变的 DefaultHasher）。
fn fnv64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// 校验后的单条 hook 事件。
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedHookEvent {
    /// 事件时间（RFC3339，UTC；payload 无时间戳 → 接收时刻 now_ms）
    pub ts: String,
    pub action: &'static str,
    /// 幂等键（三档，见模块头「写库口径」）："{source}:{event_id 或 fnv64}"
    pub ext_event_id: String,
    pub source: &'static str,
    pub session_id: String,
    pub tool_name: Option<String>,
    pub duration_ms: Option<i64>,
    pub waiting: Option<bool>,
    pub retry: Option<bool>,
    /// 写入 events.event_data 的 JSON（schema_version + 明细；无标题无正文）
    pub data: Value,
}

/// 解析 hook 事件批（整批先验后写）。`events` 为请求体 events 数组元素；
/// `now_ms` 注入时钟（测试红线；生产传真实 UTC 毫秒）。
///
/// 单条字段：
/// - `hook_event_name`（必填，事件词表映射；或 `action` 直接给规范动作）；
/// - `session_id`（必填 1-128 字符）；
/// - `timestamp`（可选 RFC3339；缺省用 now_ms —— hook payload 实测无时间戳，
///   收到时刻即 observed 证据）；
/// - `tool_name`（可选 ≤64 字符）、`duration_ms`（可选 ≥0 整数）、
///   `waiting` / `retry`（可选布尔，等待/重试标记）；
/// - `event_id`（可选字符串去重头，复审 2026-11-11：1-128 字符）——重发端
///   重试同一事件时带上，幂等键跨重发稳定；不带则按上述三档口径。
///
/// `raw`：与 events 等长的原始 payload 字节切片（幂等哈希输入）；None 时用
/// 序列化后的 Value（CLI 通路无原始字节时的等价回退——同一 payload 序列化
/// 稳定）。
pub fn parse_hook_events(
    source: &'static str,
    events: &[Value],
    now_ms: i64,
    raw: Option<&[Vec<u8>]>,
) -> Result<Vec<ParsedHookEvent>, String> {
    let mut out = Vec::with_capacity(events.len());
    for (i, ev) in events.iter().enumerate() {
        fn bad(i: usize, msg: impl Into<String>) -> String {
            format!("events[{i}]: {}", msg.into())
        }
        let ev = ev.as_object().ok_or_else(|| bad(i, "应为对象"))?;
        // 动作：hook_event_name 映射优先；否则 action 必须已是规范词表值
        let action: &'static str = if let Some(h) =
            ev.get("hook_event_name").and_then(|s| s.as_str())
        {
            map_hook_event(h).ok_or_else(|| {
                bad(i, format!("未知 hook_event_name \"{h}\"（允许: PreToolUse, PostToolUse, Notification, Stop, SessionStart, UserPromptSubmit, PermissionRequest, PostToolUseFailure）"))
            })?
        } else if let Some(a) = ev.get("action").and_then(|s| s.as_str()) {
            AI_HOOK_ACTIONS
                .iter()
                .copied()
                .find(|&c| c == a)
                .ok_or_else(|| {
                    bad(
                        i,
                        format!(
                            "未知 action \"{a}\"（允许: {}）",
                            AI_HOOK_ACTIONS.join(", ")
                        ),
                    )
                })?
        } else {
            return Err(bad(i, "缺少 hook_event_name（或 action）"));
        };
        let session_id = ev
            .get("session_id")
            .and_then(|s| s.as_str())
            .ok_or_else(|| bad(i, "缺少 session_id"))?;
        if session_id.is_empty() || session_id.len() > 128 {
            return Err(bad(i, "session_id 应为 1-128 字符"));
        }
        if session_id.chars().any(|c| c.is_control()) {
            return Err(bad(i, "session_id 含控制字符"));
        }
        let ts = match ev.get("timestamp").and_then(|s| s.as_str()) {
            Some(raw_ts) => chrono::DateTime::parse_from_rfc3339(raw_ts)
                .map_err(|_| bad(i, "timestamp 应为 RFC3339"))?
                .with_timezone(&chrono::Utc)
                .to_rfc3339(),
            None => chrono::DateTime::from_timestamp_millis(now_ms)
                .unwrap_or(chrono::DateTime::UNIX_EPOCH)
                .to_rfc3339(),
        };
        let tool_name = match ev.get("tool_name") {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) => {
                if s.is_empty() || s.len() > 64 {
                    return Err(bad(i, "tool_name 应为 1-64 字符"));
                }
                Some(s.clone())
            }
            Some(_) => return Err(bad(i, "tool_name 应为字符串")),
        };
        let duration_ms = match ev.get("duration_ms") {
            None | Some(Value::Null) => None,
            Some(Value::Number(n)) => {
                let d = n
                    .as_i64()
                    .ok_or_else(|| bad(i, "duration_ms 应为非负整数"))?;
                if d < 0 {
                    return Err(bad(i, "duration_ms 不能为负"));
                }
                Some(d)
            }
            Some(_) => return Err(bad(i, "duration_ms 应为整数")),
        };
        let waiting = match ev.get("waiting") {
            None | Some(Value::Null) => None,
            Some(Value::Bool(b)) => Some(*b),
            Some(_) => return Err(bad(i, "waiting 应为布尔")),
        };
        let retry = match ev.get("retry") {
            None | Some(Value::Null) => None,
            Some(Value::Bool(b)) => Some(*b),
            Some(_) => return Err(bad(i, "retry 应为布尔")),
        };
        let mut data = serde_json::Map::new();
        data.insert("schema_version".into(), Value::from(1));
        data.insert("source".into(), Value::from(source));
        data.insert("session_id".into(), Value::from(session_id));
        if let Some(t) = &tool_name {
            data.insert("tool_name".into(), Value::from(t.as_str()));
        }
        if let Some(d) = duration_ms {
            data.insert("duration_ms".into(), Value::from(d));
        }
        if let Some(w) = waiting {
            data.insert("waiting".into(), Value::from(w));
        }
        if let Some(r) = retry {
            data.insert("retry".into(), Value::from(r));
        }
        // 幂等键（独立复审 2026-11-11）：纯 payload 哈希无法区分「同一条
        // 事件的重发」与「第二次发生的相同事件」——payload 无 timestamp 时
        // （ts 由 now_ms 注入、不参与哈希），同一会话两条内容完全相同的真实
        // hook 事件会同 id 被吞。分三档：
        // 1. payload 显式带字符串 `event_id`（去重头）→ 直接用它做键：
        //    重发端重试同一事件时带同一 id，跨重发稳定去重；
        // 2. payload 自带 `timestamp` → 哈希 = payload 原始字节：同一条重发
        //    字节相同 → 去重；两次发生时间不同 → 字节不同 → 各自成事件；
        // 3. payload 无 timestamp（实测常态，ts=接收时刻）→ 哈希输入并入
        //    接收毫秒 now_ms：每次接收都是一次独立 observed 发生，不再把
        //    第二次发生误判成重发吞掉。重发端需要重试去重时请带 `event_id`。
        let explicit_event_id = match ev.get("event_id") {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) => {
                if s.is_empty() || s.len() > 128 {
                    return Err(bad(i, "event_id 应为 1-128 字符"));
                }
                if s.chars().any(|c| c.is_control()) {
                    return Err(bad(i, "event_id 含控制字符"));
                }
                Some(s.clone())
            }
            Some(_) => return Err(bad(i, "event_id 应为字符串")),
        };
        let ext_event_id = if let Some(eid) = explicit_event_id {
            format!("{source}:{eid}")
        } else {
            let mut id_bytes: Vec<u8> = match raw {
                Some(r) => r.get(i).cloned().unwrap_or_default(),
                None => Value::Object(ev.clone()).to_string().into_bytes(),
            };
            // 只有 payload 无自带 timestamp 时才并入接收毫秒（档 3）；有
            // timestamp 的 payload 保持纯字节哈希（档 2，重发去重可用）。
            if ev.get("timestamp").is_none() {
                id_bytes.extend_from_slice(format!("\n#recv_ms={now_ms}").as_bytes());
            }
            format!("{source}:{:016x}", fnv64(&id_bytes))
        };
        out.push(ParsedHookEvent {
            ts,
            action,
            ext_event_id,
            source,
            session_id: session_id.to_string(),
            tool_name,
            duration_ms,
            waiting,
            retry,
            data: Value::Object(data),
        });
    }
    Ok(out)
}

/// 摄取（append-only 幂等）：INSERT OR IGNORE 按 ext_event_id（migration
/// 0017 部分唯一索引）。返回 (inserted, duplicates)。
pub fn ingest_hook_events(
    conn: &Connection,
    events: &[ParsedHookEvent],
) -> rusqlite::Result<(u64, u64)> {
    let mut inserted = 0u64;
    let mut duplicates = 0u64;
    {
        let mut stmt = conn.prepare(
            "INSERT OR IGNORE INTO events \
             (timestamp, event_type, event_action, event_data, app_name, source_id, quality, ext_event_id) \
             VALUES (?1, 'ai_session', ?2, ?3, ?4, ?5, 'observed', ?6)",
        )?;
        for ev in events {
            let n = stmt.execute(params![
                ev.ts,
                ev.action,
                ev.data.to_string(),
                ev.source,
                ev.source,
                ev.ext_event_id,
            ])?;
            if n == 1 {
                inserted += 1;
            } else {
                duplicates += 1; // 幂等命中：同 payload 已落库
            }
        }
    }
    Ok((inserted, duplicates))
}

/// 请求体级校验入口（dash 与 CLI 共用；同校验同幂等路径）：
/// 校验顶层结构 → 解析批 → 返回。`body` 为完整请求体 JSON 字符串。
/// 请求体形状与 /api/ext/events 一致：`{"source":..., "events":[...]}`。
pub fn parse_hook_payload(
    body: &str,
    now_ms: i64,
) -> Result<(&'static str, Vec<ParsedHookEvent>), String> {
    let v: Value = serde_json::from_str(body).map_err(|e| {
        format!(
            "请求体不是合法 JSON（第 {} 行第 {} 列附近）",
            e.line(),
            e.column()
        )
    })?;
    let obj = v.as_object().ok_or("请求体应为 JSON 对象")?;
    let source = obj
        .get("source")
        .and_then(|s| s.as_str())
        .ok_or("缺少 source")?;
    let source = AI_HOOK_SOURCES
        .iter()
        .copied()
        .find(|&s| s == source)
        .ok_or(format!(
            "未知 source \"{source}\"（AI hook 通道允许: {}；dsh 无 hook 机制，走文件扫描）",
            AI_HOOK_SOURCES.join(", ")
        ))?;
    let events = obj
        .get("events")
        .and_then(|e| e.as_array())
        .ok_or("缺少 events 数组")?;
    if events.is_empty() {
        return Err("events 不能为空".into());
    }
    if events.len() > 200 {
        return Err("events 单批不能超过 200 条".into());
    }
    let parsed = parse_hook_events(source, events, now_ms, None)?;
    Ok((source, parsed))
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW_MS: i64 = 1_792_977_600_000; // 2026-11-02T03:00:00Z（注入基准）

    fn conn() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(crate::db::SCHEMA).unwrap();
        crate::db::run_migrations(&c).unwrap();
        c
    }

    fn ev(hook: &str, session: &str, tool: &str) -> Value {
        serde_json::json!({"hook_event_name": hook, "session_id": session, "tool_name": tool})
    }

    #[test]
    fn parses_claude_hook_payload_with_injected_now() {
        let body = serde_json::json!({
            "source": "claude_code",
            "events": [
                {"hook_event_name": "PreToolUse", "session_id": "sess-fix-1", "tool_name": "Bash", "waiting": true},
                {"hook_event_name": "PostToolUse", "session_id": "sess-fix-1", "tool_name": "Bash", "duration_ms": 4200},
                {"hook_event_name": "Notification", "session_id": "sess-fix-1"},
                {"hook_event_name": "Stop", "session_id": "sess-fix-1", "retry": false},
            ]
        })
        .to_string();
        let (source, parsed) = parse_hook_payload(&body, NOW_MS).unwrap();
        assert_eq!(source, "claude_code");
        assert_eq!(parsed.len(), 4);
        assert_eq!(parsed[0].action, "tool_call");
        assert_eq!(parsed[1].action, "tool_result");
        assert_eq!(parsed[2].action, "notification");
        assert_eq!(parsed[3].action, "session_stop");
        // 无 timestamp → 注入 now_ms（observed 接收时刻）
        for p in &parsed {
            assert_eq!(
                p.ts,
                chrono::DateTime::from_timestamp_millis(NOW_MS)
                    .unwrap()
                    .to_rfc3339()
            );
        }
        assert_eq!(parsed[0].waiting, Some(true));
        assert_eq!(parsed[1].duration_ms, Some(4200));
        assert_eq!(parsed[3].retry, Some(false));
    }

    #[test]
    fn zcode_seven_events_map_and_notification_unsupported() {
        // zcode 七事件全接
        for (hook, act) in [
            ("SessionStart", "session_start"),
            ("UserPromptSubmit", "prompt_submit"),
            ("PreToolUse", "tool_call"),
            ("PermissionRequest", "permission_request"),
            ("PostToolUse", "tool_result"),
            ("PostToolUseFailure", "tool_error"),
            ("Stop", "session_stop"),
        ] {
            let body = serde_json::json!({
                "source": "zcode",
                "events": [{"hook_event_name": hook, "session_id": "zc-fix"}]
            })
            .to_string();
            let (_, parsed) = parse_hook_payload(&body, NOW_MS).unwrap();
            assert_eq!(parsed[0].action, act, "{hook} → {act}");
        }
        // Notification：claude_code 支持、zcode 不支持——但本通道的校验是
        // 动作词表层（映射表两源共用）；zcode 端渲染的 hooks 配置不含
        // Notification（render 层如实不生成），通道本身不因源区分词表。
        let body = serde_json::json!({
            "source": "zcode",
            "events": [{"hook_event_name": "Notification", "session_id": "zc-fix"}]
        })
        .to_string();
        assert!(parse_hook_payload(&body, NOW_MS).is_ok());
    }

    #[test]
    fn dsh_source_rejected_no_hook_mechanism() {
        let body = serde_json::json!({
            "source": "dsh",
            "events": [{"hook_event_name": "Stop", "session_id": "dsh-fix"}]
        })
        .to_string();
        let err = parse_hook_payload(&body, NOW_MS).unwrap_err();
        assert!(
            err.contains("dsh 无 hook 机制"),
            "错误文案应说明 dsh 走扫描: {err}"
        );
    }

    #[test]
    fn validation_whole_batch_rejects() {
        let base =
            |e: Value| serde_json::json!({"source": "claude_code", "events": [e]}).to_string();
        // 缺 session_id
        assert!(parse_hook_payload(
            &base(serde_json::json!({"hook_event_name": "Stop"})),
            NOW_MS
        )
        .is_err());
        // 未知事件名
        assert!(parse_hook_payload(
            &base(serde_json::json!({"hook_event_name": "SubagentStop", "session_id": "s"})),
            NOW_MS
        )
        .is_err());
        // 负 duration
        assert!(parse_hook_payload(&base(serde_json::json!({"hook_event_name": "PostToolUse", "session_id": "s", "duration_ms": -1})), NOW_MS).is_err());
        // 坏 timestamp
        assert!(parse_hook_payload(&base(serde_json::json!({"hook_event_name": "Stop", "session_id": "s", "timestamp": "nope"})), NOW_MS).is_err());
        // 空批 / 超批
        assert!(parse_hook_payload("{\"source\":\"claude_code\",\"events\":[]}", NOW_MS).is_err());
        let big: Vec<Value> = (0..201)
            .map(|i| serde_json::json!({"hook_event_name": "Stop", "session_id": format!("s{i}")}))
            .collect();
        assert!(parse_hook_payload(
            &serde_json::json!({"source": "claude_code", "events": big}).to_string(),
            NOW_MS
        )
        .is_err());
    }

    #[test]
    fn idempotent_by_payload_hash() {
        let c = conn();
        let body = serde_json::json!({
            "source": "claude_code",
            "events": [ev("PreToolUse", "sess-idem", "Edit")]
        })
        .to_string();
        let (_, p1) = parse_hook_payload(&body, NOW_MS).unwrap();
        let (ins1, dup1) = ingest_hook_events(&c, &p1).unwrap();
        assert_eq!((ins1, dup1), (1, 0));
        // 同 payload 重发（同 now 注入 → 同 id）→ 幂等命中不重复写
        let (_, p2) = parse_hook_payload(&body, NOW_MS).unwrap();
        let (ins2, dup2) = ingest_hook_events(&c, &p2).unwrap();
        assert_eq!((ins2, dup2), (0, 1));
        let n: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM events WHERE event_type='ai_session'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn ingest_writes_observed_rows_with_source_id() {
        let c = conn();
        let (_, parsed) = parse_hook_payload(
            &serde_json::json!({"source": "claude_code", "events": [ev("PostToolUse", "sess-obs", "Bash")]})
                .to_string(),
            NOW_MS,
        )
        .unwrap();
        ingest_hook_events(&c, &parsed).unwrap();
        let (source_id, app_name, quality, action): (String, String, String, String) = c
            .query_row(
                "SELECT source_id, app_name, quality, event_action FROM events WHERE event_type='ai_session'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(source_id, "claude_code", "真实通道 source_id=工具源名");
        assert_eq!(app_name, "claude_code");
        assert_eq!(quality, "observed");
        assert_eq!(action, "tool_result");
    }
}
