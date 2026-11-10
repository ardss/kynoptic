//! 统一时间轴与统计报表的纯查询函数（PRD v1.0 F07 / F08，Stage 1-3）。
//!
//! 实现原先在 kynoptic-dash；因 mcp 不能反向依赖 dash（dash 已依赖 mcp，
//! 会成环），而 PRD DoD 要求 UI / CLI / MCP 三端**同源同数值**，Stage 3
//! 把两个只读纯函数下沉到 core，dash 与 MCP（及 CLI）经 re-export /
//! 直调使用同一实现。函数仍是纯只读：入参 &Connection + 注入时钟 now_ms
//! （_at 变体，测试零真实时钟依赖），不写库、不重建派生表。

use chrono::{Local, TimeZone, Utc};
use rusqlite::{params, Connection};
use serde_json::{json, Value};

/// GET /api/timeline?minutes= — 统一时间轴（PRD v1.0 F07，Stage 1）。只读
/// activity_intervals 派生表（stage0-audit §5 决策 4：UI/CLI/MCP 三端只读
/// 派生表保证数值一致；本端点不写、不重建——重建由 collector writer 在批
/// 落库成功后节流触发（independent review 2026-11-03 接线，见 collector
/// maybe_rebuild_derived）：≥90s 节流 + events 水位门控，AI 会话摄入与
/// project_rules 快照同窗生效），按轨道 A-E 分组返回区间：
/// - A 前台窗口（foreground_dwell）；
/// - B 浏览器 tab（tab_foreground_dwell）：Stage 2 F02 扩展接入前无数据源，
///   整段合成一条 unknown 区间并标注「未接入」（不隐藏）；
/// - C 在场/离场（away 区间）；
/// - D AI / 编辑器活动（Stage 3 起）：activity_intervals 的 ai_run +
///   editor_session 区间（F05 AI 三源 + F04 编辑器前台分类）。每个区间带
///   `source_id` 与 `app_name`（ai_run 的 app_name=源名 claude_code/zcode/
///   dsh，editor_session 的 app_name=编辑器进程），前端按 source 分色。
///   区间全空（该窗口无任何 AI/编辑器区间）时轨道标「未接入」（不隐藏）。
/// - E 网络遥测：本阶段无数据源，intervals 留空 + 「未接入」。
///
/// F06 项目归属：区间带 `project`、`project_source`（rule/manual/unassigned
/// 三档，0018 人工指定优先于规则）与 `project_evidence`（manual=observed
/// 直证 / rule=inferred 推导）——project 为 null 时 project_source=
/// "unassigned"，无归属证据，不猜。下钻明细（前端）展示项目名与来源。
///
/// 证据等级按 PRD §2.3 词表：quality=observed → observed（原始事件直证）；
/// estimated → inferred（推导，如与 away 相交）；unknown → unknown（无证据
/// 缺口，不是零）。区间裁剪到 [now − minutes, now] 窗口，`now_ms` 注入
/// （_at 变体，测试零真实时钟依赖，先例 api_status_at / api_timeline_at）。
/// 与既有 `?hours=` 小时桶端点共用 /api/timeline 路由：请求带 `minutes`
/// 参数即走本端点，两套口径互不掺和（HTTP 层见 route_req）。
pub fn api_unified_timeline_at(
    conn: &Connection,
    minutes: u32,
    now_ms: i64,
) -> rusqlite::Result<Value> {
    api_unified_timeline_filtered_at(conn, minutes, now_ms, &TimelineFilter::default())
}

/// 统一时间轴筛选器（F07 验收补充，2026-11-10）：按项目、来源、证据等级
/// 三维过滤区间。语义（取舍说明）：
/// - `evidence`：按 PRD §2.3 词表（observed/inferred/unknown）匹配区间
///   `evidence_level`。**用户显式选了词表之外的值必须报错**（HTTP 400），
///   而不是静默返回空表——打错字导致的「全空」会被误读为「无活动」，
///   违反「unknown 不是零」的可追溯原则；`validate()` 由 HTTP 层调用。
/// - `source`：匹配区间的 `source_id` 或 `app_name`（ai_run 的 app_name =
///   源名 claude_code/zcode/dsh，editor_session 的 source_id = vscode_ext/
///   cursor_ext——两个维度都参与匹配，调用方无需知道存储细节）。来源为
///   null 的区间（含 B 轨 unknown 占位段）在带 source 过滤时不命中：用户
///   选了来源就意味着「只要这个来源」，无来源证据的区间如实缺席（轨道
///   注明「窗口内无区间」，不伪造 unknown 段冒充命中）。
/// - `project`：精确匹配区间 `project`；unassigned（null）区间不命中。
/// - B 轨整段 unknown 占位段：**仅当**过滤条件为空、或 evidence 过滤显式
///   包含 "unknown" 且未带 source/project 过滤时保留——占位段是「无数据源」
///   声明，不是可筛数据；被过滤排除时轨道显示「窗口内无区间」，缺口仍在
///   轨道的「未接入」标注里如实可见。
///
/// 过滤是用户主动收窄视角，与「不隐藏真实缺口」不冲突：图例与轨道标注
/// （未接入/unknown 纹理）不受过滤影响，响应回显 `filters` 字段供前端
/// 与三端一致性核对。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TimelineFilter {
    pub project: Vec<String>,
    pub source: Vec<String>,
    pub evidence: Vec<String>,
}

impl TimelineFilter {
    pub fn is_empty(&self) -> bool {
        self.project.is_empty() && self.source.is_empty() && self.evidence.is_empty()
    }
    /// evidence 只接受 PRD §2.3 词表；越表值返回 Err（HTTP 层映射 400）。
    pub fn validate(&self) -> Result<(), String> {
        const VALID: [&str; 3] = ["observed", "inferred", "unknown"];
        for e in &self.evidence {
            if !VALID.contains(&e.as_str()) {
                return Err(format!(
                    "evidence 取值必须是 observed/inferred/unknown 之一，得到 {e}"
                ));
            }
        }
        Ok(())
    }
    fn matches(
        &self,
        evidence_level: &str,
        source_id: Option<&str>,
        app_name: Option<&str>,
        project: Option<&str>,
    ) -> bool {
        if !self.evidence.is_empty() && !self.evidence.iter().any(|e| e == evidence_level) {
            return false;
        }
        if !self.source.is_empty() {
            let hit = source_id.is_some_and(|s| self.source.iter().any(|f| f == s))
                || app_name.is_some_and(|a| self.source.iter().any(|f| f == a));
            if !hit {
                return false;
            }
        }
        if !self.project.is_empty() && !project.is_some_and(|p| self.project.iter().any(|f| f == p))
        {
            return false;
        }
        true
    }
}

/// 带 [`TimelineFilter`] 的统一时间轴（F07 筛选契约）；无筛选时与
/// [`api_unified_timeline_at`] 完全同值（同一实现，wrapper 传 Default）。
/// 响应新增 `filters` 回显字段（三端一致性核对用）。
pub fn api_unified_timeline_filtered_at(
    conn: &Connection,
    minutes: u32,
    now_ms: i64,
    filter: &TimelineFilter,
) -> rusqlite::Result<Value> {
    let minutes = minutes.clamp(1, 1440);
    let window_start_ms = now_ms - i64::from(minutes) * 60_000;
    // activity_intervals 的时间戳统一由 core 侧 to_rfc3339() 写入
    // （+00:00 后缀、无亚秒），字典序比较与时间序一致，窗口过滤安全。
    let fmt_ms = |ms: i64| -> String {
        chrono::DateTime::from_timestamp_millis(ms)
            .unwrap_or(chrono::DateTime::UNIX_EPOCH)
            .to_rfc3339()
    };
    let parse_ms = |ts: &str| -> i64 {
        chrono::DateTime::parse_from_rfc3339(ts)
            .map(|t| t.timestamp_millis())
            .unwrap_or(0)
    };
    // quality → 证据等级（PRD §2.3：observed / inferred / unknown 三级词表）
    let evidence_level = |quality: &str| -> &'static str {
        match quality {
            "observed" | "confirmed" => "observed",
            "estimated" | "inferred" => "inferred",
            _ => "unknown",
        }
    };
    let mut stmt = conn.prepare(
        "SELECT interval_kind, start_ts, end_ts, app_name, title_hash, quality, source_id, project, project_source, device_id \
         FROM activity_intervals \
         WHERE end_ts >= ?1 AND start_ts <= ?2 \
         ORDER BY interval_kind, start_ts",
    )?;
    let rows = stmt.query_map(params![fmt_ms(window_start_ms), fmt_ms(now_ms)], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, Option<String>>(3)?,
            r.get::<_, Option<String>>(4)?,
            r.get::<_, String>(5)?,
            r.get::<_, Option<String>>(6)?,
            r.get::<_, Option<String>>(7)?,
            r.get::<_, Option<String>>(8)?,
            r.get::<_, Option<String>>(9)?,
        ))
    })?;
    let mut track_a: Vec<Value> = Vec::new();
    let mut track_b: Vec<Value> = Vec::new();
    let mut track_c: Vec<Value> = Vec::new();
    let mut track_d: Vec<Value> = Vec::new();
    for row in rows {
        let (
            kind,
            start_ts,
            end_ts,
            app,
            title_hash,
            quality,
            source_id,
            project,
            project_source,
            device_id,
        ) = row?;
        let start = parse_ms(&start_ts).max(window_start_ms);
        let end = parse_ms(&end_ts).min(now_ms);
        if end <= start {
            continue; // 完全在窗口外的裁剪残留（理论不可达，防御）
        }
        // F07 筛选（evidence/source/project）：不命中即跳过（缺口如实缺席，
        // 见 TimelineFilter 语义注释；图例/轨道标注不受过滤影响）
        if !filter.matches(
            evidence_level(&quality),
            source_id.as_deref(),
            app.as_deref(),
            project.as_deref(),
        ) {
            continue;
        }
        // F06 来源三档 + 归属证据（一次算好，json 与 match 共用）
        let psrc_str = project_source
            .clone()
            .unwrap_or_else(|| crate::db::activity_intervals::PROJECT_SOURCE_RULE.to_string());
        let iv = json!({
            "start_ms": start,
            "end_ms": end,
            "start_ts": fmt_ms(start),
            "end_ts": fmt_ms(end),
            "duration_ms": end - start,
            "app_name": app,
            "title_hash": title_hash,
            "quality": quality,
            "evidence_level": evidence_level(&quality),
            "source_id": source_id,
            "interval_kind": kind,
            // F06：项目归属 + 来源三档（rule/manual/unassigned）。manual =
            // 人工指定（0018 project_overrides，证据 observed——直证）；
            // rule = 规则命中（证据 inferred）；unassigned = 无归属证据
            // （project NULL，不猜）。project_evidence 跟随来源：
            // manual → observed，rule → inferred，无归属 → NULL。
            "project": project,
            "project_source": if project.is_some() {
                Value::from(psrc_str.clone())
            } else {
                Value::from(crate::db::activity_intervals::PROJECT_SOURCE_UNASSIGNED)
            },
            "project_evidence": if psrc_str == crate::db::activity_intervals::PROJECT_SOURCE_MANUAL {
                Value::from("observed")
            } else if project.is_some() {
                Value::from(crate::db::activity_intervals::PROJECT_EVIDENCE)
            } else {
                Value::Null
            },
            // F11：设备归属（源事件 device_id 原样抄写；null = 未带设备标识）
            "device_id": device_id,
        });
        match kind.as_str() {
            crate::db::activity_intervals::KIND_FOREGROUND_DWELL => track_a.push(iv),
            crate::db::activity_intervals::KIND_TAB_FOREGROUND_DWELL => track_b.push(iv),
            crate::db::activity_intervals::KIND_AWAY => track_c.push(iv),
            // D 轨（Stage 3）：AI 会话 + 编辑器前台两类区间，按 source 分色
            crate::db::activity_intervals::KIND_AI_RUN
            | crate::db::activity_intervals::KIND_EDITOR_SESSION => track_d.push(iv),
            _ => {}
        }
    }
    // B 轨无数据源：整段 unknown（真实缺口，不是隐藏，也不标 0）。
    // 带过滤时占位段不是可筛数据：仅当 evidence 过滤显式含 unknown 且未带
    // source/project 过滤时保留（见 TimelineFilter 语义注释）。
    let placeholder_kept = (filter.source.is_empty() && filter.project.is_empty())
        && (filter.evidence.is_empty() || filter.evidence.iter().any(|e| e == "unknown"));
    if track_b.is_empty() && placeholder_kept {
        track_b.push(json!({
            "start_ms": window_start_ms,
            "end_ms": now_ms,
            "start_ts": fmt_ms(window_start_ms),
            "end_ts": fmt_ms(now_ms),
            "duration_ms": now_ms - window_start_ms,
            "app_name": Value::Null,
            "title_hash": Value::Null,
            "quality": "unknown",
            "evidence_level": "unknown",
            "source_id": Value::Null,
            "device_id": Value::Null,
        }));
    }
    let track = |id: &str,
                 label_en: &str,
                 label_zh: &str,
                 kind: Value,
                 connected: bool,
                 note_en: Value,
                 note_zh: Value,
                 intervals: Vec<Value>| {
        json!({
            "id": id,
            "label_en": label_en,
            "label_zh": label_zh,
            "interval_kind": kind,
            "connected": connected,
            "note_en": note_en,
            "note_zh": note_zh,
            "intervals": intervals,
        })
    };
    // D 轨（Stage 3）：ai_run + editor_session 两类区间按 source 分色；
    // 全空 = 该窗口无 AI/编辑器区间数据 → 标「未接入」（不隐藏，不标 0）
    let d_connected = !track_d.is_empty();
    let track_d_json = track(
        "D",
        "AI / editor activity",
        "AI / 编辑器活动",
        json!(["ai_run", "editor_session"]),
        d_connected,
        if d_connected {
            Value::Null
        } else {
            json!("No AI/editor intervals in window (sources: claude_code / zcode / dsh / editor foreground)")
        },
        if d_connected {
            Value::Null
        } else {
            json!("窗口内无 AI / 编辑器区间数据（来源：claude_code / zcode / dsh / 编辑器前台）——未接入或未产生区间")
        },
        track_d,
    );
    Ok(json!({
        "endpoint": "unified_timeline",
        "minutes": minutes,
        // F07 筛选回显：空数组 = 未启用该维过滤；前端与三端一致性核对用
        "filters": {
            "project": filter.project,
            "source": filter.source,
            "evidence": filter.evidence,
        },
        "window_start_ms": window_start_ms,
        "window_end_ms": now_ms,
        "window_start": fmt_ms(window_start_ms),
        "window_end": fmt_ms(now_ms),
        // F11 多设备边界（不建同步服务）：本响应只含本库（本机）的区间，
        // device_id 原样来自源事件；双机的 human_presence / 时长不相加，
        // 跨机合流由后续 MCP 工具 / 导出完成。
        "device_boundary": {
            "scope": "this_database",
            "note_en": "Intervals are per-database (one device). Durations from different devices are never summed here; cross-device merging is done by GET /api/timeline?merge=1 (per-device tracks) or via MCP/export.",
            "note_zh": "区间按库（单机）返回；双机时长不在本端点相加，跨机合流由 GET /api/timeline?merge=1（按设备分轨）或 MCP / 导出完成。",
        },
        "legend": {
            "observed": "原始事件直证 / directly evidenced by raw events",
            "inferred": "推导（如与离场区间相交）/ inferred (e.g. overlapped away)",
            "unknown": "无证据缺口，不是零 / real gap, not zero",
        },
        "tracks": [
            track("A", "Foreground windows", "前台窗口",
                json!(crate::db::activity_intervals::KIND_FOREGROUND_DWELL),
                true, Value::Null, Value::Null, track_a),
            track("B", "Browser tabs", "浏览器 Tab",
                json!(crate::db::activity_intervals::KIND_TAB_FOREGROUND_DWELL),
                false,
                json!("Not connected yet — whole track unknown until Stage 2 extension (F02)"),
                json!("未接入——Stage 2 浏览器扩展（F02）接入前整段 unknown"),
                track_b),
            track("C", "Presence / away", "在场 / 离场",
                json!(crate::db::activity_intervals::KIND_AWAY),
                true, Value::Null, Value::Null, track_c),
            track_d_json,
            track("E", "Network / telemetry", "网络 / 遥测", Value::Null, false,
                json!("No data source in this stage"),
                json!("本阶段无数据源，未接入"),
                Vec::<Value>::new()),
        ],
    }))
}

/// GET /api/reports?days=N — 统计报表（PRD v1.0 F08，Stage 3）。
/// 近 N 个本地日（默认 7，上限 90；_at 注入时钟，测试零真实时钟依赖）
/// × 维度（区间类别 foreground_dwell / ai_run / editor_session × 来源
/// source_id × 项目 project）汇总时长，毫秒。只读 activity_intervals 派生表。
///
/// 口径（复审铁律）：**estimated/inferred 段单列，不计入 observed 合计**；
/// unknown 段同样单列（unknown 是真实缺口，不是零，也不是 observed）。
/// 跨本地午夜的区间按午界拆分归日（一日一段，不整段记入起始日）。
/// project 为 null 的行归入 project="(none)"（无归属证据，不是「无项目」
/// 的零时长——标签仅用于分组展示）。每行带 `project_source`（F06 可追溯：
/// manual=人工指定 / rule=规则命中 / unassigned=无归属）。
pub fn api_reports_at(conn: &Connection, days: u32, now_ms: i64) -> rusqlite::Result<Value> {
    use std::collections::BTreeMap;
    let days = days.clamp(1, 90);
    let window_end_ms = now_ms;
    // 窗口起点 = 本地「今天 − (days−1) 天」的 00:00（本地日界，与
    // queries::local_day_range 同一意图；此处直接用 Local 计算以便对齐
    // activity_intervals 的 UTC RFC3339 时间戳做窗口过滤）
    let now_local = chrono::DateTime::from_timestamp_millis(now_ms)
        .unwrap_or(chrono::DateTime::UNIX_EPOCH)
        .with_timezone(&Local);
    let first_day = (now_local.date_naive() - chrono::Duration::days(i64::from(days) - 1))
        .and_hms_opt(0, 0, 0)
        .and_then(|n| Local.from_local_datetime(&n).single())
        .map(|t| t.with_timezone(&Utc).timestamp_millis())
        .unwrap_or(now_ms - i64::from(days) * 86_400_000);
    let window_start_ms = first_day;

    let fmt_ms = |ms: i64| -> String {
        chrono::DateTime::from_timestamp_millis(ms)
            .unwrap_or(chrono::DateTime::UNIX_EPOCH)
            .to_rfc3339()
    };
    let parse_ms = |ts: &str| -> i64 {
        chrono::DateTime::parse_from_rfc3339(ts)
            .map(|t| t.timestamp_millis())
            .unwrap_or(0)
    };
    // quality → 三桶：observed（observed/confirmed）/ estimated（estimated/
    // inferred，单列不并入）/ unknown（其余）。返回值 0/1/2 桶下标。
    let bucket_of = |quality: &str| -> usize {
        match quality {
            "observed" | "confirmed" => 0,
            "estimated" | "inferred" => 1,
            _ => 2,
        }
    };

    // (date, device, kind, source, project, project_source) -> [observed, estimated, unknown] 毫秒
    // device 用原始 device_id（None → "(unknown)"，仅分组标签，如实缺省）
    type ReportKey = (String, String, String, String, String, String);
    let mut agg: BTreeMap<ReportKey, [i64; 3]> = BTreeMap::new();
    let mut stmt = conn.prepare(
        "SELECT interval_kind, start_ts, end_ts, quality, source_id, app_name, project, project_source, device_id \
         FROM activity_intervals \
         WHERE end_ts > ?1 AND start_ts < ?2 \
           AND interval_kind IN ('foreground_dwell', 'ai_run', 'editor_session')",
    )?;
    let rows = stmt.query_map(
        params![fmt_ms(window_start_ms), fmt_ms(window_end_ms)],
        |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, Option<String>>(5)?,
                r.get::<_, Option<String>>(6)?,
                r.get::<_, Option<String>>(7)?,
                r.get::<_, Option<String>>(8)?,
            ))
        },
    )?;
    for row in rows.flatten() {
        let (
            kind,
            start_ts,
            end_ts,
            quality,
            source_id,
            app_name,
            project,
            project_source,
            device_id,
        ) = row;
        let start = parse_ms(&start_ts).max(window_start_ms);
        let end = parse_ms(&end_ts).min(window_end_ms);
        if end <= start {
            continue;
        }
        // ai_run 的展示来源 = app_name（claude_code/zcode/dsh），其余 =
        // source_id（windows/browser/ai）；ai_run 的 app_name 就是源名。
        let source = app_name
            .clone()
            .filter(|a| !a.is_empty())
            .or_else(|| source_id.clone())
            .unwrap_or_else(|| "unknown".to_string());
        let project = project.unwrap_or_else(|| "(none)".to_string());
        // F06 可追溯：来源三档（manual/rule/unassigned）随行返回；无归属
        // （(none)）恒 unassigned——库里 NULL 是"无证据"，不虚构字段值。
        let project_source = if project == "(none)" {
            crate::db::activity_intervals::PROJECT_SOURCE_UNASSIGNED.to_string()
        } else {
            project_source
                .unwrap_or_else(|| crate::db::activity_intervals::PROJECT_SOURCE_RULE.to_string())
                .to_string()
        };
        let device = device_id.unwrap_or_else(|| "(unknown)".to_string());
        // 跨本地午界拆分：逐段 [seg_start, seg_end) 落入所属本地日。
        let mut seg_start = start;
        while seg_start < end {
            let seg_local = chrono::DateTime::from_timestamp_millis(seg_start)
                .unwrap_or(chrono::DateTime::UNIX_EPOCH)
                .with_timezone(&Local);
            let next_midnight_local = seg_local
                .date_naive()
                .succ_opt()
                .and_then(|d| d.and_hms_opt(0, 0, 0))
                .and_then(|n| Local.from_local_datetime(&n).single())
                .map(|t| t.with_timezone(&Utc).timestamp_millis())
                .unwrap_or(i64::MAX);
            let seg_end = end.min(next_midnight_local);
            if seg_end > seg_start {
                let date = seg_local.date_naive().format("%Y-%m-%d").to_string();
                let e = agg
                    .entry((
                        date,
                        device.clone(),
                        kind.clone(),
                        source.clone(),
                        project.clone(),
                        project_source.clone(),
                    ))
                    .or_insert([0, 0, 0]);
                e[bucket_of(&quality)] += seg_end - seg_start;
            }
            seg_start = seg_end;
            if next_midnight_local == i64::MAX {
                break;
            }
        }
    }

    let mut out_rows: Vec<Value> = Vec::new();
    let mut devices: Vec<String> = Vec::new();
    for ((date, device, kind, source, project, project_source), b) in &agg {
        if !devices.contains(device) {
            devices.push(device.clone());
        }
        out_rows.push(json!({
            "date": date,
            "device_id": device,
            "kind": kind,
            "source": source,
            "project": project,
            // F06 可追溯：manual（人工指定）/ rule（规则命中）/ unassigned（无归属）
            "project_source": project_source,
            // observed 合计只含 observed 桶（复审口径：estimated 单列不并入）
            "observed_ms": b[0],
            "estimated_ms": b[1],
            "unknown_ms": b[2],
        }));
    }
    // totals 口径（独立复审 2026-11-03）：editor_session 是 foreground_dwell
    // 同一时段的分类视图（rebuild 时从 dwell 直接复制生成，编辑器 app 同时
    // 写两种 kind 各一行），合计再求和会把编辑器时段计两次。行级明细保留
    // editor_session（按 kind 分类查看是它的用途），**totals 排除它**——
    // observed/estimated/unknown 三桶都不计入，合计 = 真实窗口占用。
    let totals = {
        let mut t = [0i64; 3];
        for ((_, _, kind, _, _, _), b) in &agg {
            if kind == "editor_session" {
                continue;
            }
            for i in 0..3 {
                t[i] += b[i];
            }
        }
        t
    };
    Ok(json!({
        "endpoint": "reports",
        "days": days,
        "window_start_ms": window_start_ms,
        "window_end_ms": window_end_ms,
        "window_start": fmt_ms(window_start_ms),
        "window_end": fmt_ms(window_end_ms),
        "note": "observed 合计只含 observed 段；estimated（含 inferred）与 unknown 单列、不计入 observed 合计；editor_session 是 foreground_dwell 的分类视图，totals 不计入（行级明细保留）避免编辑器时段计两次；project=(none) 表示无归属证据（未命中规则），不是零时长 / observed totals count observed segments only; estimated (incl. inferred) and unknown are listed separately and not counted into observed totals; editor_session is a categorical view of foreground_dwell and is excluded from totals (kept in rows) so editor time is not double counted; project=(none) means no attribution evidence, not zero.",
        // F11 多设备边界：行按 device_id 分组（device_id="(unknown)" 表示源
        // 事件未带设备标识）；totals 只覆盖本库（单机）。双机的 human_presence
        // 不相加——跨机时长在本端点不合并，合流由后续 MCP / 导出完成。
        "device_boundary": {
            "scope": "this_database",
            "note_en": "Rows are grouped per device_id; totals cover this database (one device) only. human_presence from two machines is never summed here; merging happens later via MCP/export.",
            "note_zh": "行按 device_id 分组；totals 只覆盖本库（单机）。双机 human_presence 不相加，跨机合流由后续 MCP / 导出完成。",
        },
        "devices": devices,
        "totals": {
            "observed_ms": totals[0],
            "estimated_ms": totals[1],
            "unknown_ms": totals[2],
        },
        "rows": out_rows,
    }))
}
