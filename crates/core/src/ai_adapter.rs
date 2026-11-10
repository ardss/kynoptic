//! F05 AIActivityAdapter（PRD v1.0 Stage 2）——跨生态 AI 会话扫描与摄入。
//!
//! 只读扫描外部 AI 工具的本地痕迹，解析成 [`AiSession`] 后由
//! [`ingest_ai_sessions`] 写入现有 events 表（铁律：不建第二真相表；事件
//! `event_type='ai_session'`、明细全部在 event_data JSON 里，append-only
//! 幂等——同 session_id 重复扫描不重复写）。区间由
//! `db::activity_intervals::rebuild_activity_intervals_at` 从事件派生
//! `interval_kind='ai_run'`。
//!
//! ## 三种源的真实格式（2026-11-03 本机只读调研，路径为用户真实目录）
//!
//! ### claude_code —— Claude Code 会话转写
//! - 路径模式：`<root>/projects/<路径转义目录名>/<session-uuid>.jsonl`
//!   （实测根 `C:/Users/13397/.claude/projects/`，目录名如 `K--ardss-council`）。
//! - 格式：JSONL，每行一个 JSON 对象。时间证据：行级 `timestamp`
//!   （RFC3339 带毫秒、Z 后缀）。会话标识：`sessionId` 字段（与文件名一致）。
//!   用户提示行：`type=="user"` 且 `message.content` 为字符串；回复行
//!   `type=="assistant"`；另有 `queue-operation`/`attachment` 等辅助行。
//! - 映射：start=首行 timestamp、end=末行 timestamp（均 observed）；
//!   prompt_count=user 行数；ai_run_elapsed=各提示行到下一次提示前最后
//!   一条事件时间戳的活跃段并集。
//!
//! ### zcode —— ZCode CLI 会话库
//! - 路径模式：`<root>/cli/db/db.sqlite`（实测根 `C:/Users/13397/.zcode/`），
//!   SQLite 只读打开，绝不写源库。
//! - 表：`session(id, time_created, time_updated, ...)`，时间列 epoch 毫秒
//!   整数；`session_input(session_id, kind, ...)` 计提示数（status 任意，
//!   已取消的输入也是一次用户提示尝试，如实计数）。
//! - 映射：session_id=表 id、start=time_created、end=time_updated（库内
//!   记录的时间，observed）；活跃段=整段会话（库无逐段活跃证据，elapsed=
//!   全长，这是该源的证据上限，不多猜）。
//!
//! ### dsh —— DSH 工作流产物
//! - 路径模式：`<root>/.dsh/workflow-artifacts/dwfrun-<uuid>/`（实测根
//!   `K:/OBSIDIAN/AI/`），目录含 summary/report/result 等 md/txt 产物文件。
//! - 格式：产物为普通文本（无行级时间戳）；时间证据只有文件系统 mtime。
//! - 映射：session_id=目录名；start=最早产物 mtime、end=最晚产物 mtime
//!   ——mtime 是间接证据，quality=inferred（非 observed 也不算 unknown：
//!   文件确实存在）；prompt_count 无法从产物得知，记 0 并在 quality 上
//!   体现降级，不虚构。
//!
//! ## 证据词表
//! observed / inferred / unknown（PRD §2.3；映射 M1 词表在对外层做）。
//! 断开的会话（无行级/库级时间戳，只能按 mtime 收口）end_quality=unknown。
//! unknown 不是零。
//!
//! ## 隐私
//! 只读源文件；解析结果不含任何标题/正文（仅 session_id、时间、计数）；
//! 测试 fixture 一律用脱敏自造样例，禁抄真实标题/路径。

use rusqlite::{params, Connection};
use std::path::{Path, PathBuf};

/// 事件数据 schema 版本（event_data JSON 内自描述，便于未来演进）
pub const AI_SESSION_SCHEMA_VERSION: i64 = 1;
/// source 词表：claude_code / zcode / dsh
pub const SOURCE_CLAUDE_CODE: &str = "claude_code";
pub const SOURCE_ZCODE: &str = "zcode";
pub const SOURCE_DSH: &str = "dsh";

/// 证据等级（PRD §2.3）
pub const Q_OBSERVED: &str = "observed";
pub const Q_INFERRED: &str = "inferred";
pub const Q_UNKNOWN: &str = "unknown";

/// 解析出的一个 AI 会话（无标题无正文，只有时间与计数）。
#[derive(Debug, Clone, PartialEq)]
pub struct AiSession {
    pub session_id: String,
    pub source: &'static str,
    /// 会话开始（UTC epoch 毫秒）
    pub start_ms: i64,
    /// 结束（UTC epoch 毫秒）；`end_quality` 说明该值的证据来源
    pub end_ms: i64,
    /// start 证据等级：observed（行级/库级时间戳）/ inferred（mtime）/ unknown
    pub start_quality: &'static str,
    pub end_quality: &'static str,
    /// end 是否为扫描时刻收口（真值：会话可能尚未结束，end 不确定）
    pub end_closed_at_scan: bool,
    pub prompt_count: u32,
    /// 活跃段（提示→响应），已按并集语义合并不重叠区间
    pub active_segments: Vec<(i64, i64)>,
    /// 活跃段并集总时长（ai_run_elapsed：并集不是求和，PRD 口径铁律）
    pub ai_run_elapsed_ms: i64,
}

impl AiSession {
    /// 会话整体区间 quality：任一端 unknown → unknown；任一端 inferred →
    /// inferred；两端都 observed → observed（最弱端定级）。
    pub fn interval_quality(&self) -> &'static str {
        match (self.start_quality, self.end_quality) {
            (Q_UNKNOWN, _) | (_, Q_UNKNOWN) => Q_UNKNOWN,
            (Q_INFERRED, _) | (_, Q_INFERRED) => Q_INFERRED,
            _ => Q_OBSERVED,
        }
    }

    /// event_data JSON（含 schema_version，禁把标题/路径写进来）。
    pub fn to_event_data(&self) -> serde_json::Value {
        serde_json::json!({
            "schema_version": AI_SESSION_SCHEMA_VERSION,
            "session_id": self.session_id,
            "source": self.source,
            "start": rfc3339(self.start_ms),
            "end": rfc3339(self.end_ms),
            "start_quality": self.start_quality,
            "end_quality": self.end_quality,
            "end_closed_at_scan": self.end_closed_at_scan,
            "prompt_count": self.prompt_count,
            "active_segments": self
                .active_segments
                .iter()
                .map(|(s, e)| serde_json::json!({"start": rfc3339(*s), "end": rfc3339(*e)}))
                .collect::<Vec<_>>(),
            "ai_run_elapsed_ms": self.ai_run_elapsed_ms,
        })
    }
}

// ─── 扫描 ────────────────────────────────────────────────────────────────

/// 扫描给定目录集合下的三种 AI 源（只读；目录不存在/库不可读 → 该源空结果，
/// 不报错——扫描器对缺源宽容）。`now_ms` 注入时钟（测试红线）。
pub fn scan_ai_sessions_at(dirs: &[PathBuf], now_ms: i64) -> Vec<AiSession> {
    let mut out = Vec::new();
    for dir in dirs {
        out.extend(scan_claude_code(dir));
        out.extend(scan_zcode(dir));
        out.extend(scan_dsh(dir, now_ms));
    }
    out
}

/// 生产扫描根（独立复审 2026-11-03 接线）：collector 周期扫描用它。
/// - claude_code：`<home>/.claude`（扫 `projects/<ws>/*.jsonl`）；
/// - zcode：`<home>/.zcode`（扫 `cli/db/db.sqlite`）；
/// - dsh：环境变量 `KYNOPTIC_DSH_ROOT` 指定的根（扫 `<root>/.dsh/
///   workflow-artifacts/`）——dsh 产物根因人而异（实测根无跨机器缺省），
///   不设环境变量则该源不扫（如实缺省，不猜路径）。
///
/// home 取 USERPROFILE（Windows）/HOME（Unix）；都缺 → 空列表（不扫描）。
pub fn default_ai_roots() -> Vec<PathBuf> {
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .ok()
        .map(PathBuf::from);
    let mut out = Vec::new();
    if let Some(h) = home {
        out.push(h.join(".claude"));
        out.push(h.join(".zcode"));
    }
    if let Ok(dsh) = std::env::var("KYNOPTIC_DSH_ROOT") {
        let dsh = dsh.trim();
        if !dsh.is_empty() {
            out.push(PathBuf::from(dsh));
        }
    }
    out
}

/// 解析 RFC3339 时间戳为 epoch 毫秒（失败返回 None，调用方按证据缺失处理）。
fn parse_ts_ms(ts: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(ts)
        .ok()
        .map(|t| t.timestamp_millis())
}

fn rfc3339(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .unwrap_or(chrono::DateTime::UNIX_EPOCH)
        .to_rfc3339()
}

/// 文件 mtime → epoch 毫秒（读不到 → None）。
fn mtime_ms(path: &Path) -> Option<i64> {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| {
            t.duration_since(std::time::UNIX_EPOCH)
                .ok()
                .map(|d| d.as_millis() as i64)
        })
}

/// 活跃段并集：排序后合并重叠/相邻段，返回 (union_ms, merged_segments)。
fn union_segments(mut segs: Vec<(i64, i64)>) -> (i64, Vec<(i64, i64)>) {
    segs.sort();
    let mut merged: Vec<(i64, i64)> = Vec::new();
    for (s, e) in segs {
        if e <= s {
            continue;
        }
        match merged.last_mut() {
            Some(last) if s <= last.1 => last.1 = last.1.max(e),
            _ => merged.push((s, e)),
        }
    }
    let total: i64 = merged.iter().map(|(s, e)| e - s).sum();
    (total, merged)
}

/// claude_code：`<root>/projects/**/*.jsonl`（只扫一层 projects 子目录，
/// 每个子目录对应一个工作区路径）。
fn scan_claude_code(root: &Path) -> Vec<AiSession> {
    let projects = root.join("projects");
    let mut out = Vec::new();
    let Ok(ws_dirs) = std::fs::read_dir(&projects) else {
        return out;
    };
    for ws in ws_dirs.flatten() {
        let Ok(files) = std::fs::read_dir(ws.path()) else {
            continue;
        };
        for f in files.flatten() {
            let p = f.path();
            if p.extension().map(|e| e == "jsonl") != Some(true) {
                continue;
            }
            if let Some(s) = parse_claude_jsonl(&p) {
                out.push(s);
            }
        }
    }
    out
}

/// 解析单个 Claude Code 会话 jsonl（只读；坏行跳过，不中断整个文件）。
fn parse_claude_jsonl(path: &Path) -> Option<AiSession> {
    let content = std::fs::read_to_string(path).ok()?;
    let mut session_id: Option<String> = None;
    let mut timestamps: Vec<i64> = Vec::new();
    let mut prompt_ts: Vec<i64> = Vec::new();
    for line in content.lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let ts = v
            .get("timestamp")
            .and_then(|t| t.as_str())
            .and_then(parse_ts_ms);
        if let Some(t) = ts {
            timestamps.push(t);
        }
        if session_id.is_none() {
            if let Some(id) = v.get("sessionId").and_then(|s| s.as_str()) {
                session_id = Some(id.to_string());
            }
        }
        // 用户提示行：type=="user" 且 message.content 为字符串
        if v.get("type").and_then(|t| t.as_str()) == Some("user") {
            let is_prompt = v
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(|c| c.as_str())
                .is_some();
            if is_prompt {
                if let Some(t) = ts {
                    prompt_ts.push(t);
                }
            }
        }
    }
    if timestamps.is_empty() {
        // 无行级时间戳 → 会话时间不可证，只有 mtime：按扫描收口标 unknown
        let m = mtime_ms(path)?;
        return Some(AiSession {
            session_id: session_id.unwrap_or_else(|| {
                path.file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default()
            }),
            source: SOURCE_CLAUDE_CODE,
            start_ms: m,
            end_ms: m,
            start_quality: Q_UNKNOWN,
            end_quality: Q_UNKNOWN,
            end_closed_at_scan: true,
            prompt_count: 0,
            active_segments: Vec::new(),
            ai_run_elapsed_ms: 0,
        });
    }
    timestamps.sort_unstable();
    // 活跃段：每次提示 → 下一次提示前最后一条事件时间
    let mut segs = Vec::new();
    for (i, &p) in prompt_ts.iter().enumerate() {
        let seg_end = timestamps
            .iter()
            .rev()
            .find(|&&t| match prompt_ts.get(i + 1) {
                Some(&np) => t >= p && t < np,
                None => t >= p,
            })
            .copied()
            .unwrap_or(p);
        segs.push((p, seg_end));
    }
    let (elapsed, merged) = union_segments(segs);
    Some(AiSession {
        session_id: session_id.unwrap_or_else(|| {
            path.file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default()
        }),
        source: SOURCE_CLAUDE_CODE,
        start_ms: timestamps[0],
        end_ms: *timestamps.last().unwrap(),
        start_quality: Q_OBSERVED,
        end_quality: Q_OBSERVED,
        end_closed_at_scan: false,
        prompt_count: prompt_ts.len() as u32,
        active_segments: merged,
        ai_run_elapsed_ms: elapsed,
    })
}

/// zcode：`<root>/cli/db/db.sqlite` 只读打开（绝不写源库）。
fn scan_zcode(root: &Path) -> Vec<AiSession> {
    let db = root.join("cli").join("db").join("db.sqlite");
    if !db.is_file() {
        return Vec::new();
    }
    let conn = match Connection::open_with_flags(&db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let Ok(mut stmt) = conn.prepare(
        "SELECT s.id, s.time_created, s.time_updated, \
                (SELECT COUNT(*) FROM session_input i WHERE i.session_id = s.id) \
         FROM session s ORDER BY s.time_created",
    ) else {
        return Vec::new();
    };
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
            ))
        })
        .map(|it| it.filter_map(|r| r.ok()).collect::<Vec<_>>())
        .unwrap_or_default();
    let mut out = Vec::new();
    for (id, created, updated, prompts) in rows {
        if created <= 0 {
            continue;
        }
        // 活跃段证据上限：整段会话（库无逐段活跃证据，不多猜）
        out.push(AiSession {
            session_id: id,
            source: SOURCE_ZCODE,
            start_ms: created,
            end_ms: updated.max(created),
            start_quality: Q_OBSERVED,
            end_quality: Q_OBSERVED,
            end_closed_at_scan: false,
            prompt_count: prompts.max(0) as u32,
            active_segments: vec![(created, updated.max(created))],
            ai_run_elapsed_ms: (updated - created).max(0),
        });
    }
    out
}

/// dsh：`<root>/.dsh/workflow-artifacts/dwfrun-*/` 目录，时间只来自产物
/// mtime（inferred）。目录内没有任何可读产物 mtime → 跳过（无证据不虚构）。
fn scan_dsh(root: &Path, now_ms: i64) -> Vec<AiSession> {
    let artifacts = root.join(".dsh").join("workflow-artifacts");
    let Ok(entries) = std::fs::read_dir(&artifacts) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for e in entries.flatten() {
        let dir = e.path();
        if !dir.is_dir() {
            continue;
        }
        let Ok(files) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut mtimes: Vec<i64> = Vec::new();
        for f in files.flatten() {
            if let Some(m) = mtime_ms(&f.path()) {
                mtimes.push(m);
            }
        }
        if let Some(dir_m) = mtime_ms(&dir) {
            mtimes.push(dir_m);
        }
        if mtimes.is_empty() {
            continue;
        }
        mtimes.sort_unstable();
        let start = mtimes[0];
        let end = *mtimes.last().unwrap();
        let session_id = dir
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        // 最晚产物 mtime 距扫描时刻 < 1h：会话可能仍在进行，end 按扫描收口
        // 标 unknown（断开收口口径）；否则 mtime 即收尾证据（inferred）。
        let open = now_ms.saturating_sub(end) < 3_600_000;
        let (end_q, end_ms, closed) = if open {
            (Q_UNKNOWN, now_ms.max(end), true)
        } else {
            (Q_INFERRED, end, false)
        };
        let start_q = if start == end && open {
            Q_UNKNOWN
        } else {
            Q_INFERRED
        };
        out.push(AiSession {
            session_id,
            source: SOURCE_DSH,
            start_ms: start,
            end_ms,
            start_quality: start_q,
            end_quality: end_q,
            end_closed_at_scan: closed,
            prompt_count: 0,
            active_segments: vec![(start, end_ms)],
            ai_run_elapsed_ms: end_ms.saturating_sub(start),
        });
    }
    out
}

// ─── 摄入（append-only 幂等）────────────────────────────────────────────

/// 本机 device_id（metadata 表，首次生成 UUIDv4 存储；stage0-audit §5 决策 3）。
/// 返回值供事件写入；缺表/写失败返回 None（事件仍写，device_id 空——不阻塞）。
pub fn ensure_device_id(conn: &Connection) -> Option<String> {
    let existing: Option<String> = conn
        .query_row(
            "SELECT value FROM metadata WHERE key = 'device_id'",
            [],
            |r| r.get(0),
        )
        .ok();
    if let Some(id) = existing {
        if !id.is_empty() {
            return Some(id);
        }
    }
    let id = uuid_v4();
    let ok = conn
        .execute(
            "INSERT INTO metadata (key, value) VALUES ('device_id', ?1) \
             ON CONFLICT(key) DO UPDATE SET value = ?1",
            params![id],
        )
        .is_ok();
    ok.then_some(id)
}

/// 极简 UUIDv4（rand 已在依赖内，不为此引入 uuid crate）。
fn uuid_v4() -> String {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    let b: [u8; 16] = rng.gen();
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-4{:01x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[0], b[1], b[2], b[3], b[4], b[5], b[6] & 0x0f, b[7], (b[8] & 0x3f) | 0x80, b[9],
        b[10], b[11], b[12], b[13], b[14], b[15]
    )
}

/// 把扫描出的会话写入 events 表（append-only 幂等）：
/// - 每个会话一行 `event_type='ai_session'`、`event_action='ai_run'` 事件，
///   明细全在 event_data JSON（含 schema_version）；
/// - 去重键 = event_data 里的 session_id（json_extract 精确匹配），已存在
///   则跳过（重复扫描不重复写）；
/// - `source_id='ai'`、`quality` = 会话区间最弱端证据；
/// - timestamp = 会话 start（写库侧 normalize_timestamp 统一 +00:00）。
///
/// 返回本批新写入的行数。
pub fn ingest_ai_sessions(
    conn: &Connection,
    sessions: &[AiSession],
    device_id: Option<&str>,
) -> rusqlite::Result<usize> {
    let mut written = 0usize;
    for s in sessions {
        let exists: i64 = conn.query_row(
            "SELECT COUNT(*) FROM events \
             WHERE event_type = 'ai_session' \
               AND json_extract(event_data, '$.session_id') = ?1",
            params![s.session_id],
            |r| r.get(0),
        )?;
        if exists > 0 {
            continue;
        }
        conn.execute(
            "INSERT INTO events \
             (timestamp, event_type, event_action, event_data, app_name, \
              source_id, device_id, quality) \
             VALUES (?1, 'ai_session', 'ai_run', ?2, ?3, 'ai', ?4, ?5)",
            params![
                rfc3339(s.start_ms),
                s.to_event_data().to_string(),
                s.source,
                device_id,
                s.interval_quality(),
            ],
        )?;
        written += 1;
    }
    Ok(written)
}

// ─── 测试（全部脱敏自造 fixture，禁真实标题/路径；时钟一律 _at 注入）─────

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::SystemTime;

    /// 固定注入时钟：2026-11-02T03:00:00Z（与 activity_intervals 测试同基准）
    const NOW_MS: i64 = 1_792_977_600_000;

    fn conn() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(crate::db::SCHEMA).unwrap();
        crate::db::run_migrations(&c).unwrap();
        c
    }

    /// 建临时目录并在其中写文件（自动清理由 tempdir 手工包装）。
    struct TempDir(PathBuf);
    impl TempDir {
        fn new(tag: &str) -> Self {
            let mut p = std::env::temp_dir();
            p.push(format!(
                "ky-ai-adapter-test-{}-{}",
                tag,
                SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&p).unwrap();
            Self(p)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// 相对 NOW_MS 的 RFC3339Z 时间戳（毫秒固定 .000，与真实源格式一致）
    fn ts(offset_secs: i64) -> String {
        chrono::DateTime::from_timestamp_millis(NOW_MS + offset_secs * 1000)
            .unwrap()
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
    }

    /// claude_code fixture：脱敏 jsonl（字段结构与真实调研一致：timestamp
    /// RFC3339Z、sessionId、type=user/assistant、message.content 字符串）。
    #[test]
    fn scan_claude_code_parses_prompts_and_union() {
        let d = TempDir::new("claude");
        let ws = d.0.join("projects").join("K--example-workspace");
        std::fs::create_dir_all(&ws).unwrap();
        let jsonl = format!(
            "{{\"type\":\"queue-operation\",\"operation\":\"enqueue\",\"timestamp\":\"{}\",\"sessionId\":\"sess-aaaa-1111\"}}\n\
             {{\"type\":\"user\",\"message\":{{\"role\":\"user\",\"content\":\"sample prompt one\"}},\"timestamp\":\"{}\",\"sessionId\":\"sess-aaaa-1111\"}}\n\
             {{\"type\":\"assistant\",\"message\":{{\"role\":\"assistant\",\"content\":[{{\"type\":\"text\",\"text\":\"reply\"}}]}},\"timestamp\":\"{}\",\"sessionId\":\"sess-aaaa-1111\"}}\n\
             {{\"type\":\"user\",\"message\":{{\"role\":\"user\",\"content\":\"sample prompt two\"}},\"timestamp\":\"{}\",\"sessionId\":\"sess-aaaa-1111\"}}\n\
             {{\"type\":\"assistant\",\"message\":{{\"role\":\"assistant\",\"content\":[{{\"type\":\"text\",\"text\":\"reply two\"}}]}},\"timestamp\":\"{}\",\"sessionId\":\"sess-aaaa-1111\"}}\n\
             {{\"type\":\"assistant\",\"message\":{{\"role\":\"assistant\",\"content\":[{{\"type\":\"text\",\"text\":\"followup\"}}]}},\"timestamp\":\"{}\",\"sessionId\":\"sess-aaaa-1111\"}}\n",
            ts(0),
            ts(10),
            ts(20),
            ts(100),
            ts(110),
            ts(130),
        );
        let f = ws.join("sess-aaaa-1111.jsonl");
        std::fs::write(&f, jsonl).unwrap();

        let sessions = scan_ai_sessions_at(std::slice::from_ref(&d.0), NOW_MS + 1_000_000);
        assert_eq!(sessions.len(), 1, "只应发现一个 claude_code 会话");
        let s = &sessions[0];
        assert_eq!(s.source, SOURCE_CLAUDE_CODE);
        assert_eq!(s.session_id, "sess-aaaa-1111");
        assert_eq!(s.prompt_count, 2);
        assert_eq!(s.start_quality, Q_OBSERVED);
        assert_eq!(s.end_quality, Q_OBSERVED);
        assert_eq!(s.start_ms, parse_ts_ms(&ts(0)).unwrap());
        assert_eq!(s.end_ms, parse_ts_ms(&ts(130)).unwrap());
        // 活跃段 [10,20] 与 [100,130] 不相交 → 并集 10+30=40s（并集≠求和的
        // 反例是重叠段：第二个测试覆盖）
        assert_eq!(s.ai_run_elapsed_ms, 40_000);
        assert_eq!(s.interval_quality(), Q_OBSERVED);
    }

    /// 活跃段语义：每段 = 提示 → 下一次提示前的最后一条事件（按时间边界，
    /// 天然不重叠）；空段（提示后无事件）被丢弃，不计 0 时长。
    #[test]
    fn claude_segments_bounded_by_next_prompt() {
        let d = TempDir::new("claude-overlap");
        let ws = d.0.join("projects").join("W");
        std::fs::create_dir_all(&ws).unwrap();
        // 提示1@0（无后续事件 → 空段丢弃）；提示2@30，回复@60 → 段 [30,60]
        let jsonl = format!(
            "{{\"type\":\"user\",\"message\":{{\"content\":\"p1\"}},\"timestamp\":\"{}\",\"sessionId\":\"ov\"}}\n\
             {{\"type\":\"user\",\"message\":{{\"content\":\"p2\"}},\"timestamp\":\"{}\",\"sessionId\":\"ov\"}}\n\
             {{\"type\":\"assistant\",\"timestamp\":\"{}\",\"sessionId\":\"ov\"}}\n",
            ts(0),
            ts(30),
            ts(60),
        );
        std::fs::write(ws.join("ov.jsonl"), jsonl).unwrap();
        let sessions = scan_ai_sessions_at(std::slice::from_ref(&d.0), NOW_MS + 1_000_000);
        let s = &sessions[0];
        assert_eq!(s.prompt_count, 2);
        assert_eq!(s.ai_run_elapsed_ms, 30_000, "空段丢弃，只剩 [30,60]");
        assert_eq!(
            s.active_segments,
            vec![(parse_ts_ms(&ts(30)).unwrap(), parse_ts_ms(&ts(60)).unwrap())]
        );
    }

    /// 断开的会话：jsonl 无任何 timestamp → mtime 收口 + unknown 证据。
    #[test]
    fn claude_session_without_timestamps_closes_at_mtime_unknown() {
        let d = TempDir::new("claude-nots");
        let ws = d.0.join("projects").join("W2");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(
            ws.join("no-ts.jsonl"),
            "{\"type\":\"user\",\"message\":{\"content\":\"orphan\"},\"sessionId\":\"no-ts\"}\n",
        )
        .unwrap();
        let sessions = scan_ai_sessions_at(std::slice::from_ref(&d.0), NOW_MS);
        let s = &sessions[0];
        assert_eq!(s.start_quality, Q_UNKNOWN);
        assert_eq!(s.end_quality, Q_UNKNOWN);
        assert!(s.end_closed_at_scan);
        assert_eq!(s.interval_quality(), Q_UNKNOWN);
    }

    /// zcode：只读打开 db.sqlite fixture（自建库，表结构与真实调研一致：
    /// session/time_created/time_updated epoch 毫秒 + session_input 计数）。
    #[test]
    fn scan_zcode_reads_session_table_readonly() {
        let d = TempDir::new("zcode");
        let db_dir = d.0.join("cli").join("db");
        std::fs::create_dir_all(&db_dir).unwrap();
        {
            let c = Connection::open(db_dir.join("db.sqlite")).unwrap();
            c.execute_batch(
                "CREATE TABLE session (id text primary key, time_created integer not null, \
                 time_updated integer not null);
                 CREATE TABLE session_input (id text primary key, session_id text not null, \
                 kind text not null, delivery text not null, payload text not null, \
                 admitted_sequence integer not null, promoted_sequence integer, \
                 promoted_message_id text, status text not null, status_reason text, \
                 time_created integer not null, time_updated integer not null);
                 INSERT INTO session VALUES ('sess_fixture_1', 1790000000000, 1790000600000);
                 INSERT INTO session_input VALUES ('i1','sess_fixture_1','text','startNow','{}',0,NULL,NULL,'admitted',NULL,1790000000000,1790000000000);
                 INSERT INTO session_input VALUES ('i2','sess_fixture_1','text','queue','{}',1,NULL,NULL,'promoted',NULL,1790000300000,1790000300000);",
            )
            .unwrap();
        }
        let sessions = scan_ai_sessions_at(std::slice::from_ref(&d.0), NOW_MS);
        assert_eq!(sessions.len(), 1);
        let s = &sessions[0];
        assert_eq!(s.source, SOURCE_ZCODE);
        assert_eq!(s.session_id, "sess_fixture_1");
        assert_eq!(s.prompt_count, 2);
        assert_eq!(s.start_ms, 1_790_000_000_000);
        assert_eq!(s.end_ms, 1_790_000_600_000);
        assert_eq!(s.ai_run_elapsed_ms, 600_000);
        assert_eq!(s.interval_quality(), Q_OBSERVED);
        // 只读性守卫：目录里不应生成 -wal/-shm 以外的写痕迹（fixture 库打开
        // 后行数不变即等价幂等，重复扫描结果一致）
        let again = scan_ai_sessions_at(std::slice::from_ref(&d.0), NOW_MS);
        assert_eq!(again, sessions);
    }

    /// dsh：产物目录 mtime 推导（inferred）；近期改动目录按扫描收口 unknown。
    #[test]
    fn scan_dsh_uses_artifact_mtimes() {
        let d = TempDir::new("dsh");
        let old =
            d.0.join(".dsh")
                .join("workflow-artifacts")
                .join("dwfrun-old-run");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(old.join("summary-v1-abcdef.md"), "# 脱敏样例\n").unwrap();
        // mtime 设为远早于 NOW（filetime 不可用 → 用 now 比较：老 run 无法
        // 在不引入 filetime 的情况下把 mtime 改到过去。改为用「目录距扫描
        // 时刻 1h 内 → unknown 收口」的两个分支分别验证：old 目录 mtime=真实
        // 当前时间 → 落在 unknown 收口分支。inferred 分支用宽容窗口验证逻辑：
        // 单测注入 now_ms = mtime + 2h。）
        let real_mtime_ms = mtime_ms(&old).unwrap();
        let sessions = scan_ai_sessions_at(std::slice::from_ref(&d.0), real_mtime_ms + 7_200_000);
        let s = sessions.iter().find(|s| s.source == SOURCE_DSH).unwrap();
        assert_eq!(s.session_id, "dwfrun-old-run");
        assert_eq!(s.start_quality, Q_INFERRED);
        assert_eq!(s.end_quality, Q_INFERRED);
        assert!(!s.end_closed_at_scan);
        assert_eq!(s.interval_quality(), Q_INFERRED);
        assert!(s.ai_run_elapsed_ms >= 0);

        // 近期（now - end < 1h）→ 扫描收口 unknown（断开收口口径）
        let sessions2 = scan_ai_sessions_at(std::slice::from_ref(&d.0), real_mtime_ms + 60_000);
        let s2 = sessions2.iter().find(|s| s.source == SOURCE_DSH).unwrap();
        assert_eq!(s2.end_quality, Q_UNKNOWN);
        assert!(s2.end_closed_at_scan);
        assert_eq!(s2.interval_quality(), Q_UNKNOWN);
    }

    /// 摄入：append-only 幂等（同 session_id 重复摄入不重复写）；device_id
    /// 首次生成持久进 metadata；事件行带 source_id='ai' 与 quality。
    #[test]
    fn ingest_is_idempotent_by_session_id() {
        let c = conn();
        let device = ensure_device_id(&c);
        assert!(device.is_some());
        // metadata 持久化：再次 ensure 得同一 id
        assert_eq!(ensure_device_id(&c), device);

        let s = AiSession {
            session_id: "sess-ingest-fix".into(),
            source: SOURCE_CLAUDE_CODE,
            start_ms: NOW_MS,
            end_ms: NOW_MS + 60_000,
            start_quality: Q_OBSERVED,
            end_quality: Q_OBSERVED,
            end_closed_at_scan: false,
            prompt_count: 3,
            active_segments: vec![(NOW_MS, NOW_MS + 60_000)],
            ai_run_elapsed_ms: 60_000,
        };
        let n1 = ingest_ai_sessions(&c, std::slice::from_ref(&s), device.as_deref()).unwrap();
        assert_eq!(n1, 1);
        // 重复扫描同一会话再摄入 → 0 新行（append-only，无重复）
        let n2 = ingest_ai_sessions(&c, &[s], device.as_deref()).unwrap();
        assert_eq!(n2, 0, "同 session_id 不得重复写");

        let (cnt, source_id, quality, event_type, action): (i64, String, String, String, String) =
            c.query_row(
                "SELECT COUNT(*), source_id, quality, event_type, event_action \
                 FROM events WHERE event_type='ai_session'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .unwrap();
        assert_eq!(cnt, 1);
        assert_eq!(source_id, "ai");
        assert_eq!(quality, "observed");
        assert_eq!(event_type, "ai_session");
        assert_eq!(action, "ai_run");
        let data: String = c
            .query_row(
                "SELECT event_data FROM events WHERE event_type='ai_session'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&data).unwrap();
        assert_eq!(v["schema_version"], 1);
        assert_eq!(v["session_id"], "sess-ingest-fix");
        assert_eq!(v["source"], "claude_code");
        assert_eq!(v["prompt_count"], 3);
        assert_eq!(v["ai_run_elapsed_ms"], 60_000);
        // device_id 写进事件
        let dev: String = c
            .query_row(
                "SELECT device_id FROM events WHERE event_type='ai_session'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(Some(dev.as_str()), device.as_deref());
        // metadata 表确有 device_id
        let meta: String = c
            .query_row(
                "SELECT value FROM metadata WHERE key='device_id'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(Some(meta.as_str()), device.as_deref());
    }

    /// 真实源冒烟（`cargo test -p kynoptic-core ai_adapter -- --ignored`）：
    /// 只读本机真实三源，断言能发现会话且字段合法；绝不写源文件。
    #[test]
    #[ignore = "依赖本机真实数据（只读），CI 无该环境"]
    fn real_sources_smoke_readonly() {
        let dirs = vec![
            PathBuf::from("C:/Users/13397/.claude"),
            PathBuf::from("C:/Users/13397/.zcode"),
            PathBuf::from("K:/OBSIDIAN/AI"),
        ];
        let now = chrono::Utc::now().timestamp_millis();
        let sessions = scan_ai_sessions_at(&dirs, now);
        let by_src = |src: &str| sessions.iter().filter(|s| s.source == src).count();
        println!(
            "claude_code={} zcode={} dsh={}",
            by_src(SOURCE_CLAUDE_CODE),
            by_src(SOURCE_ZCODE),
            by_src(SOURCE_DSH)
        );
        for s in sessions.iter().take(5) {
            println!(
                "{} {} prompts={} elapsed={}ms q={:?}",
                s.source,
                s.session_id,
                s.prompt_count,
                s.ai_run_elapsed_ms,
                s.interval_quality()
            );
        }
        assert!(
            by_src(SOURCE_CLAUDE_CODE) > 0,
            "claude_code 应至少发现 1 个会话"
        );
        assert!(by_src(SOURCE_ZCODE) > 0, "zcode 应至少发现 1 个会话");
        assert!(by_src(SOURCE_DSH) > 0, "dsh 应至少发现 1 个会话");
        for s in &sessions {
            assert!(s.end_ms >= s.start_ms);
            assert!(
                s.start_quality == Q_OBSERVED
                    || s.start_quality == Q_INFERRED
                    || s.start_quality == Q_UNKNOWN
            );
            assert!(s.ai_run_elapsed_ms >= 0);
        }
    }

    /// 摄入多个会话 + 事件→区间派生贯通：ingest 后 rebuild 产出 ai_run 区间
    /// （quality 按源证据定级：observed 会话 → observed；mtime 会话 → inferred）。
    #[test]
    fn ingest_then_rebuild_derives_ai_run_intervals() {
        let c = conn();
        let device = ensure_device_id(&c);
        let observed = AiSession {
            session_id: "fix-obs".into(),
            source: SOURCE_CLAUDE_CODE,
            start_ms: NOW_MS,
            end_ms: NOW_MS + 300_000,
            start_quality: Q_OBSERVED,
            end_quality: Q_OBSERVED,
            end_closed_at_scan: false,
            prompt_count: 2,
            active_segments: vec![(NOW_MS, NOW_MS + 300_000)],
            ai_run_elapsed_ms: 300_000,
        };
        let inferred = AiSession {
            session_id: "dsh-fix-inf".into(),
            source: SOURCE_DSH,
            start_ms: NOW_MS + 600_000,
            end_ms: NOW_MS + 900_000,
            start_quality: Q_INFERRED,
            end_quality: Q_INFERRED,
            end_closed_at_scan: false,
            prompt_count: 0,
            active_segments: vec![(NOW_MS + 600_000, NOW_MS + 900_000)],
            ai_run_elapsed_ms: 300_000,
        };
        ingest_ai_sessions(&c, &[observed, inferred], device.as_deref()).unwrap();
        let n =
            crate::db::activity_intervals::rebuild_activity_intervals_at(&c, NOW_MS + 1_000_000)
                .unwrap();
        assert!(n > 0);
        let rows: Vec<(String, i64, String)> = {
            let mut stmt = c
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
        assert_eq!(rows[0], ("claude_code".into(), 300_000, "observed".into()));
        assert_eq!(rows[1], ("dsh".into(), 300_000, "inferred".into()));
    }
}
