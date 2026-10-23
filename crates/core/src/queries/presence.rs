//! 人在场 / 自动化 分钟级分类与桥接 —— 全站唯一权威实现。
//!
//! 权威口径（与 dash overview `minute_classification` 同一 SQL）：
//! - human（人在场）= 真实键鼠：`keys - injected_keys + clicks - injected_clicks > 0`；
//! - auto（自动化）= 注入输入：`injected_keys + injected_clicks > 0`；
//! - 混合分钟两者同时为真，**同时计入** presence 与 automation（mixed 单独返回）；
//! - 人在场输入含滚轮滚动（Wave21 定案：滚轮=人在主动阅读，属"真实键鼠"；
//!   注入滚轮 `$.injected_scroll_ticks` 不计入 human、计入 auto）；
//! - 相邻在场分钟间隙 <= bridge 分钟按"无输入阅读"桥接（读 settings 的
//!   `presence_bridge_minutes`，0-15，默认 2）。桥接**可以**跨纯自动化分钟
//!   延伸（Wave21 定案：人在机器前看 agent 干活时自己不敲键盘，仍属在场；
//!   纯无人值守场景由 unattended 指标扣减表达）；
//! - raw 模式（opt-in 逐键）按 event_data 的 "injected" 字段扣减注入输入
//!   （keyboard_hook / mouse_hook 落库时写入 LLKHF/LLMHF_INJECTED 判定）。
//!
//! 消费方：crates/dash（overview / timeline）、crates/cli（presence 子命令）。
//! 禁止在消费方再写第四份口径——需要改动请只改这里。

use chrono::{Local, Utc};
use rusqlite::{params, Connection};

/// 分钟级分类（overview 与 timeline 共用同一 SQL/判定）：
/// 纯 SQL 聚合出每个本地分钟桶的 (human, auto) 命中。
/// `start`/`end` 为 UTC RFC3339 边界；`off` 为 SQLite datetime() 的本地偏移
/// 修饰符（如 `"+28800 seconds"`，测试可注入历史时刻的偏移）。
/// 返回 (本地分钟桶 "YYYY-MM-DD HH:MM", human, auto)。
pub fn minute_classification(
    conn: &Connection,
    start: &str,
    end: &str,
    off: &str,
) -> Result<Vec<(String, bool, bool)>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT substr(datetime(timestamp, ?1), 1, 16) AS minute_bucket, \
                    MAX(MAX(COALESCE(json_extract(event_data,'$.keys'),0) - COALESCE(json_extract(event_data,'$.injected_keys'),0), 0) + MAX(COALESCE(json_extract(event_data,'$.clicks'),0) - COALESCE(json_extract(event_data,'$.injected_clicks'),0), 0) + MAX(COALESCE(json_extract(event_data,'$.scroll_ticks'),0) - COALESCE(json_extract(event_data,'$.injected_scroll_ticks'),0), 0)) > 0, \
                    MAX(COALESCE(json_extract(event_data,'$.injected_keys'),0) + COALESCE(json_extract(event_data,'$.injected_clicks'),0) + COALESCE(json_extract(event_data,'$.injected_scroll_ticks'),0)) > 0 \
             FROM events \
             WHERE timestamp >= ?2 AND timestamp < ?3 AND event_action = 'input_agg' \
               AND json_valid(event_data) \
             GROUP BY minute_bucket",
        )
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map(params![off, start, end], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, bool>(1)?,
                r.get::<_, bool>(2)?,
            ))
        })
        .map_err(|e| e.to_string())?;
    Ok(super::collect_rows_warn(
        rows,
        "presence.minute_classification",
    ))
}

/// 桥接计数：排序去重后的分钟序列里，相邻间隙 <= gap 分钟按"无输入阅读"
/// 桥接成连续在场段，返回覆盖的分钟总数。
///
/// 间隙守卫（审查修复 2026-09）：仅当 `m - prev <= gap + 1` 时才按
/// 「端点分钟本身 1」+「桥接补洞 min(diff-1, gap)」计入（diff>=1 时与
/// `min(diff, gap+1)` 恒等）；超过桥接阈值的长空洞（人根本不在场）只计
/// 当前在场分钟本身——旧实现无条件 `min(diff, gap+1)` 会把 544 分钟的
/// 无人空洞折算成 16 分钟"在场"。连续分钟序列（diff=1）仍每步 +1，
/// 端点分钟不丢（保留旧契约：不得改成裸 `min(gap, diff-1)`）。
pub fn bridge_count(sorted_minutes: &[i64], gap: i64) -> i64 {
    if sorted_minutes.is_empty() {
        return 0;
    }
    let mut total = 1i64;
    let mut prev = sorted_minutes[0];
    for &m in &sorted_minutes[1..] {
        let diff = m - prev;
        if diff <= gap + 1 {
            // 可桥接：端点 1 + 补洞 min(diff-1, gap)
            total += diff.max(1);
        } else {
            // 间隙超过桥接阈值：空洞不算在场，只计当前分钟
            total += 1;
        }
        prev = m;
    }
    total
}

/// 单日三指标结果（人在场桥接后）。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PresenceDay {
    /// 人在场分钟（真实键鼠 + 桥接；混合分钟计入）
    pub presence_minutes: i64,
    /// 自动化分钟（有注入输入的分钟数；混合分钟计入）
    pub automation_minutes: i64,
    /// 混合分钟（human 与 auto 同时为真的分钟数）
    pub mixed_minutes: i64,
    /// 当日首次人在场（本地 "HH:MM"；无人在场分钟时为 None）
    pub first_activity: Option<String>,
    /// 当日最近人在场（本地 "HH:MM"；无人在场分钟时为 None）
    pub last_activity: Option<String>,
}

/// 纪元分钟 → 本地日期串（多日分桶用）。每个纪元分钟恰属一个本地日
/// （本地午夜与分钟边界对齐），DST 回拨日两趟重复本地小时各得不同纪元
/// 分钟，不会被互吞。
fn local_date_of_minute(m: i64) -> Option<String> {
    chrono::DateTime::from_timestamp(m * 60, 0).map(|t| {
        t.with_timezone(&Local)
            .date_naive()
            .format("%Y-%m-%d")
            .to_string()
    })
}

/// 纪元分钟格式化为本地 "HH:MM"（首末活动输出用）。
fn fmt_epoch_hm(m: i64) -> Option<String> {
    chrono::DateTime::from_timestamp(m * 60, 0)
        .map(|t| t.with_timezone(&Local).format("%H:%M").to_string())
}

/// 整窗分钟命中（单次 SQL）：`[start, end)` 为 UTC RFC3339 边界，返回
/// (UTC 纪元分钟, human 命中, auto 命中)，按 SQL 行序、未排序。
///
/// 性能定案（perf 审查 2026-09）：长跨度（30-365 天）查询此前按日循环、
/// 每日独立重扫当日 events 做 JSON 提取，成本随天数严格线性（年跨度热
/// 13.7s）。现改为一次取全窗、再按本地日分桶（见 [`collect_day_hits`]），
/// SQL 与判定口径逐字不变。
fn collect_minute_hits_window(conn: &Connection, start: &str, end: &str) -> Vec<(i64, bool, bool)> {
    // minute 模式：按 UTC 纪元分钟分桶聚合（strftime('%s') 解析 RFC3339 的
    // 时区后缀，两趟重复本地小时得到不同纪元分钟，不再互吞）。
    let Ok(mut stmt) = conn.prepare(
        "SELECT CAST(strftime('%s', timestamp) AS INTEGER) / 60 AS minute_epoch, \
                MAX(MAX(COALESCE(json_extract(event_data,'$.keys'),0) - COALESCE(json_extract(event_data,'$.injected_keys'),0), 0) + MAX(COALESCE(json_extract(event_data,'$.clicks'),0) - COALESCE(json_extract(event_data,'$.injected_clicks'),0), 0) + MAX(COALESCE(json_extract(event_data,'$.scroll_ticks'),0) - COALESCE(json_extract(event_data,'$.injected_scroll_ticks'),0), 0)) > 0, \
                MAX(COALESCE(json_extract(event_data,'$.injected_keys'),0) + COALESCE(json_extract(event_data,'$.injected_clicks'),0) + COALESCE(json_extract(event_data,'$.injected_scroll_ticks'),0)) > 0 \
         FROM events \
         WHERE timestamp >= ?1 AND timestamp < ?2 AND event_action = 'input_agg' \
           AND json_valid(event_data) \
         GROUP BY minute_epoch",
    ) else {
        return Vec::new();
    };
    let Ok(rows) = stmt.query_map(params![start, end], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, bool>(1)?,
            r.get::<_, bool>(2)?,
        ))
    }) else {
        return Vec::new();
    };
    super::collect_rows_warn(rows, "presence.minute_hits_window")
}

/// timestamp → UTC 纪元分钟（raw 侧共用解析）：优先整串 RFC3339（同时接受
/// 'Z' 与 '+08:00' 后缀，存量/外部写入的带偏移行不错算时区）；不可解析的
/// 旧行退回 naive-UTC 路径（与写路径 normalize_timestamp 同口径）；仍失败
/// 返回 None（调用方跳过）。
fn rfc3339_or_naive_to_epoch_min(ts: &str) -> Option<i64> {
    use chrono::TimeZone;
    if let Ok(t) = chrono::DateTime::parse_from_rfc3339(ts) {
        Some(t.with_timezone(&Utc).timestamp() / 60)
    } else if let Ok(t) = chrono::NaiveDateTime::parse_from_str(ts, "%Y-%m-%dT%H:%M") {
        Some(Utc.from_utc_datetime(&t).timestamp() / 60)
    } else {
        None
    }
}

/// 整窗 raw 模式（opt-in 逐键）人在场纪元分钟（未排序去重）。
///
/// 排除注入事件——hook 已把 LLKHF_INJECTED / LLMHF_INJECTED 归一化为
/// event_data 的 "injected" 布尔（见 keyboard_hook.rs / mouse_hook.rs 落库
/// 字段），注入输入不得计入人在场；旧数据无该字段时 COALESCE 为 0，仍按
/// 人算（与旧行为一致）。
/// 取整条 timestamp 解析（DISTINCT 已去重；回拨日两趟重复本地小时的
/// UTC 串本就不同，不会互吞），折算成纪元分钟。
/// 审查修复：此前 substr 截前 16 字符后强当无时区 UTC 解析——存量/外部
/// 写入的带偏移行（如 '+08:00'）会被整体错算一个时区差。现在优先按
/// RFC3339 整串解析（parse_from_rfc3339 同时接受 'Z' 与 '+08:00' 后缀）
/// 转 UTC；不可解析的旧行退回原 naive-UTC 路径，与写路径
/// normalize_timestamp 同口径。
pub fn raw_human_minutes_window(conn: &Connection, start: &str, end: &str) -> Vec<i64> {
    raw_flag_minutes_window(conn, start, end, false)
}

/// 整窗 raw 模式**自动化**纪元分钟（联合审查 P2/raw 条件 P1 补齐）：注入
/// 事件单独成桶。raw 模式此前只有 human 侧（排除注入），纯注入分钟的
/// 自动化指标恒 0——「机器替人值班」指标在 raw 档完全失明（SendKeys 跑
/// 一整夜，自动化=0 分钟）。与 [`raw_human_minutes_window`] 同解析口径，
/// 只取 injected=1 的行。
pub fn raw_auto_minutes_window(conn: &Connection, start: &str, end: &str) -> Vec<i64> {
    raw_flag_minutes_window(conn, start, end, true)
}

/// [`raw_human_minutes_window`] / [`raw_auto_minutes_window`] 共用实现：
/// `injected=true` 取注入行（自动化桶），false 取非注入行（人在场桶）。
fn raw_flag_minutes_window(
    conn: &Connection,
    start: &str,
    end: &str,
    injected: bool,
) -> Vec<i64> {
    let flag = if injected { 1 } else { 0 };
    let label = if injected {
        "presence.raw_auto_minutes"
    } else {
        "presence.raw_human_minutes"
    };
    let Ok(mut stmt) = conn.prepare(
        "SELECT DISTINCT timestamp FROM events \
         WHERE event_action IN ('press','click','scroll') \
           AND timestamp >= ?1 AND timestamp < ?2 \
           AND COALESCE(json_extract(event_data,'$.injected'), 0) = ?3",
    ) else {
        return Vec::new();
    };
    let Ok(rows) = stmt.query_map(params![start, end, flag], |r| r.get::<_, String>(0)) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for ts in super::collect_rows_warn(rows, label) {
        if let Some(epoch_min) = rfc3339_or_naive_to_epoch_min(&ts) {
            out.push(epoch_min);
        }
    }
    out
}

/// 按本地日分桶的共用取数核心：对给定日期列表**一次**扫描整窗（分钟 SQL +
/// raw SQL 各一条），返回 每日 → (human 纪元分钟, auto 纪元分钟, mixed 分钟数)，
/// 均未排序去重。语义与逐日调用旧 `collect_minute_hits` 完全一致——分钟
/// 归属按纪元分钟的本地日期判定，与旧实现按日 `[start, end)` 过滤等价。
fn collect_day_hits(
    conn: &Connection,
    local_days: &[String],
) -> std::collections::HashMap<String, (Vec<i64>, Vec<i64>, i64)> {
    let mut out: std::collections::HashMap<String, (Vec<i64>, Vec<i64>, i64)> =
        std::collections::HashMap::new();
    // 整窗边界 = 各日范围的 min/max；全部不可解析时不发查询
    let mut wstart: Option<String> = None;
    let mut wend: Option<String> = None;
    for (s, e) in local_days.iter().filter_map(|d| super::local_day_range(d)) {
        wstart = Some(match wstart {
            Some(cur) if cur.as_str() <= s.as_str() => cur,
            _ => s,
        });
        wend = Some(match wend {
            Some(cur) if cur.as_str() >= e.as_str() => cur,
            _ => e,
        });
    }
    let (Some(wstart), Some(wend)) = (wstart, wend) else {
        return out;
    };
    let bucket = |m: i64| local_date_of_minute(m);
    for (mepoch, human_hit, auto_hit) in collect_minute_hits_window(conn, &wstart, &wend) {
        let Some(day) = bucket(mepoch) else { continue };
        let e = out.entry(day).or_default();
        if human_hit {
            e.0.push(mepoch);
        }
        if auto_hit {
            if human_hit {
                e.2 += 1;
            }
            e.1.push(mepoch);
        }
    }
    for mepoch in raw_human_minutes_window(conn, &wstart, &wend) {
        if let Some(day) = bucket(mepoch) {
            out.entry(day).or_default().0.push(mepoch);
        }
    }
    // 联合审查（raw 自动化桶）：注入行单独计入自动化——纯注入分钟不再
    // 失明。raw 模式的混合分钟（同人同机）两桶都进：与 minute 模式的
    // human∩auto 双计语义对齐（e.2 mixed 计数仍只由分钟 SQL 分支维护）。
    for mepoch in raw_auto_minutes_window(conn, &wstart, &wend) {
        if let Some(day) = bucket(mepoch) {
            out.entry(day).or_default().1.push(mepoch);
        }
    }
    out
}

/// 指定本地日的三指标全管线（分类 + raw 补充 + 桥接）——全站唯一入口。
///
/// `local_day` 为 `YYYY-MM-DD` 本地日期；`bridge_min` 为桥接阈值
/// （调用方从 settings 的 `presence_bridge_minutes` 读入，本函数钳到 0-15）。
///
/// 分钟键口径（审查：DST 回拨丢失修复）：一律用 **UTC 纪元分钟**
/// （`timestamp / 60`）作为去重与桥接的线性分钟 id——此前 minute 模式按本地
/// "HH:MM" 当日分钟数、raw 模式按 UTC 分钟串，回拨日的 +1 重复小时两趟
/// 会被折叠到同一个本地钟面值而丢分钟。纪元分钟对两趟天然唯一、严格单调，
/// 去重与桥接数学完全不变；首末活动在输出时再格式化回本地 "HH:MM"。
pub fn classify_minutes(conn: &Connection, local_day: &str, bridge_min: u32) -> PresenceDay {
    classify_minutes_multi(
        conn,
        std::slice::from_ref(&local_day.to_string()),
        bridge_min,
    )
    .into_iter()
    .next()
    .map(|(_, d)| d)
    .unwrap_or_default()
}

/// 多日版 [`classify_minutes`]（长跨度查询路径）：N 天循环改为**一次**整窗
/// SQL，再按本地日分桶跑同一套桥接/首末活动算法。返回顺序与 `local_days`
/// 一致；逐日结果与逐日调用 [`classify_minutes`] 完全等价。
pub fn classify_minutes_multi(
    conn: &Connection,
    local_days: &[String],
    bridge_min: u32,
) -> Vec<(String, PresenceDay)> {
    // cap=None 时按契约恒 Some（见 classify_minutes_multi_capped 文档），
    // unwrap 只是让「不截断」路径保持原签名；None 分支不可达。
    classify_minutes_multi_capped(conn, local_days, bridge_min, None)
        .into_iter()
        .map(|(d, p)| (d, p.expect("cap=None 截断不可用分支不可达")))
        .collect()
}

/// 多日版 + 当日分钟上限截断：`cap_minute_of_day` 为 Some(n) 时，只统计各本地日
/// 当日 00:00 起 n 分钟以内的人/自动分钟（即「昨日同时段」基线——与今天已流逝
/// 的分钟数对齐后再比，消除早晨「今天必然大幅低于昨天全天」的系统性误导，
/// 语义与 MCP get_summary 的 same_time 基线一致）。桥接链也只在截断窗口内
/// 计算：昨天 23:59 的在场不会隔着截断点桥进今天已流逝时段的口径。
///
/// 截断起点解析失败（DST 春推日本地午夜钟面不存在等）时该日返回 **None**——
/// 审查修复：旧实现静默放弃截断、把昨日**全天**值当「同时段」基线交给调用方，
/// 正是要消除的误导以无声方式回归。None = 「截断不可用」，调用方应回退全天
/// 口径并如实标注，而不是拿到假基线。
pub fn classify_minutes_multi_capped(
    conn: &Connection,
    local_days: &[String],
    bridge_min: u32,
    cap_minute_of_day: Option<i64>,
) -> Vec<(String, Option<PresenceDay>)> {
    let hits = collect_day_hits(conn, local_days);
    let bridge = i64::from(bridge_min.min(15));
    local_days
        .iter()
        .map(|day| {
            let cap_cut = cap_minute_of_day.map(|cap| {
                // 各日本地日起点（UTC 纪元分钟）——hits 已按本地日分桶，
                // 同桶分钟必然 ≥ 当日本地 00:00，减去起点即当日钟面分钟。
                super::local_day_range(day)
                    .and_then(|(s, _)| chrono::DateTime::parse_from_rfc3339(&s).ok())
                    .map(|t| t.timestamp() / 60)
                    .map(|ds| ds + cap.max(0))
            });
            // 截断被要求但起点解析失败 → 该日截断不可用，不产出假基线。
            if matches!(cap_cut, Some(None)) {
                return (day.clone(), None);
            }
            let until = match cap_cut {
                Some(Some(u)) => Some(u),
                _ => None,
            };
            let (mut human, mut automation, mixed) = hits
                .get(day)
                .cloned()
                .unwrap_or_else(|| (Vec::new(), Vec::new(), 0));
            if let Some(until) = until {
                human.retain(|&m| m < until);
                automation.retain(|&m| m < until);
            }
            human.sort_unstable();
            human.dedup();
            automation.sort_unstable();
            automation.dedup();
            // 审查修复：mixed 必须与 human/automation 同口径——截断后从两列
            // 交集重算，否则昨日 23 点的混合分钟会混进「昨日同时段」基线。
            let mixed = if until.is_some() {
                let mut h: std::collections::HashSet<_> = human.iter().collect();
                automation.iter().filter(|m| h.remove(*m)).count() as i64
            } else {
                mixed
            };
            let presence = bridge_count(&human, bridge);
            // 首末人在场：取人侧最小/最大纪元分钟，格式化回本地 "HH:MM"。
            let first_activity = human.first().copied().and_then(fmt_epoch_hm);
            let last_activity = human.last().copied().and_then(fmt_epoch_hm);
            (
                day.clone(),
                Some(PresenceDay {
                    presence_minutes: presence,
                    automation_minutes: automation.len() as i64,
                    mixed_minutes: mixed,
                    first_activity,
                    last_activity,
                }),
            )
        })
        .collect()
}

/// 指定本地日的**人在场纪元分钟序列**（升序去重）——全站唯一权威实现。
///
/// 供马拉松等「在场连续性」类异常检测取数：注入输入（injected_keys /
/// injected_clicks / 注入滚轮）不计入、含滚轮滚动，与 [`classify_minutes`]
/// / 总览「今日在场」完全同源——自动化脚本把键盘敲得再响也撑不起「人在场」。
pub fn human_minutes_by_date(conn: &Connection, local_day: &str) -> Vec<i64> {
    human_minutes_by_days(conn, &[local_day.to_string()])
        .into_iter()
        .next()
        .map(|(_, m)| m)
        .unwrap_or_default()
}

/// 多日版 [`human_minutes_by_date`]：整窗一次 SQL 后按本地日分桶，
/// 返回顺序与 `local_days` 一致，逐日结果与逐日调用完全等价。
pub fn human_minutes_by_days(conn: &Connection, local_days: &[String]) -> Vec<(String, Vec<i64>)> {
    let hits = collect_day_hits(conn, local_days);
    local_days
        .iter()
        .map(|day| {
            let mut human = hits.get(day).map(|h| h.0.clone()).unwrap_or_default();
            human.sort_unstable();
            human.dedup();
            (day.clone(), human)
        })
        .collect()
}

/// 前台应用驻留的「停摆间隔」封顶：相邻窗口切换间隔超过该秒数（如关机、
/// 暂停采集、看门狗重启造成的空洞）不归属任何应用——8 小时关机不能变成
/// 某应用 8 小时前台驻留。CLI presence / dash overview / dash report /
/// MCP get_top_apps 共用同一常量与实现。
pub const FG_DWELL_STALL_CAP_SECS: i64 = 2 * 3600;

/// 前台驻留口径说明（供 API/MCP 响应显式自述口径）。
pub const FG_DWELL_METHOD: &str = "按窗口切换间隔累计前台时长，剔除超过 2 小时的停摆间隔";

/// 窗口 `[start, end)`（UTC RFC3339）内各前台应用的驻留秒数——
/// **前台 dwell 的全站唯一权威实现**（此前 CLI presence、dash overview /
/// report、MCP get_top_apps 各写一份，封顶策略不一致导致同一问题两个答案）。
///
/// 口径：应用驻留 = 本次 switch 到下次 switch 的间隔；相邻间隔超过
/// `stall_cap_secs`（停摆）按封顶归属而非整体丢弃（联合审查 P1-6：单
/// switch 日整段驻留全丢——3 小时连续使用记 0 秒，出现「用了 3 小时却无
/// top 应用」；停摆防挂机语义保留为归属上限）；倒序/漂移产生的负间隔按
/// 0 计；`credit_tail` 为 true 时末次 switch 记到窗口上界（不超过封顶）
/// ——dash overview / MCP 分段视图传 true，CLI presence 汇总保持 false。
/// 返回 (app, dwell_secs) 按 dwell 降序、app 升序。
pub fn foreground_dwell_by_app(
    conn: &Connection,
    start: &str,
    end: &str,
    stall_cap_secs: i64,
    credit_tail: bool,
) -> Vec<(String, i64)> {
    let mut dwell: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
    let Ok(mut stmt) = conn.prepare(
        "SELECT timestamp, COALESCE(NULLIF(app_name,''), NULLIF(window_title,''), '(unknown)') \
         FROM events \
         WHERE event_type = 'window' AND event_action = 'switch' \
           AND timestamp >= ?1 AND timestamp < ?2 ORDER BY timestamp",
    ) else {
        return Vec::new();
    };
    let rows: Vec<(String, String)> = stmt
        .query_map(params![start, end], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })
        .map(|v| v.flatten().collect())
        .unwrap_or_default();
    let parse = |t: &str| chrono::DateTime::parse_from_rfc3339(t).ok();
    let parsed: Vec<(chrono::DateTime<chrono::FixedOffset>, &String)> = rows
        .iter()
        .filter_map(|(ts, app)| parse(ts).map(|t| (t, app)))
        .collect();
    for pair in parsed.windows(2) {
        // 采集停摆（暂停/看门狗杀/关机）的超长间隔按封顶归属而非整体丢弃
        // （联合审查 P1-6：旧实现 `continue` 整段清零——单 switch 日 3 小时
        // 连续使用在前台驻留里完全不可见）；防挂机语义保留：归属上限 =
        // stall_cap_secs。负间隔按 0（时间戳时区偏移漂移时字符串序可能与
        // 瞬时序相反）。
        let secs = ((pair[1].0 - pair[0].0).num_seconds().max(0)).min(stall_cap_secs);
        *dwell.entry(pair[0].1.clone()).or_insert(0) += secs;
    }
    if credit_tail {
        if let (Some(last), Some(end_t)) = (parsed.last(), parse(end)) {
            let secs = ((end_t - last.0).num_seconds().max(0)).min(stall_cap_secs);
            *dwell.entry(last.1.clone()).or_insert(0) += secs;
        }
    }
    let mut out: Vec<(String, i64)> = dwell.into_iter().collect();
    out.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    out
}

/// 当前 UTC 时刻的本地偏移修饰符（timeline 等需要把窗口边界与桶换算对齐
/// 到同一偏移时用；与 [`super::local_offset_modifier`] 同口径）。
pub fn local_offset_modifier_now() -> String {
    let secs = Utc::now().with_timezone(&Local).offset().local_minus_utc() as i64;
    format!(
        "{}{} seconds",
        if secs >= 0 { "+" } else { "-" },
        secs.abs()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;

    /// raw 模式带偏移时间戳修复回归：'2026-09-23T01:00:00+08:00' 这类存量/
    /// 外部写入行不得被 substr 前 16 字符强当 UTC（旧实现整体错一个时区差）。
    /// 期望值由同一时区的本地钟面推出，测试在任意时区下都成立。
    #[test]
    fn raw_mode_offset_rows_are_not_timezone_shifted() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(crate::db::schema::SCHEMA).unwrap();
        let day = crate::queries::today_local_str();
        let offset = Local::now().format("%:z").to_string(); // 如 "+08:00"
        let ts = format!("{day}T01:00:00{offset}");
        conn.execute(
            "INSERT INTO events (timestamp, event_type, event_action) VALUES (?1, 'keyboard', 'press')",
            params![&ts],
        )
        .unwrap();
        let d = classify_minutes(&conn, &day, 2);
        assert_eq!(
            d.first_activity.as_deref(),
            Some("01:00"),
            "带偏移 {offset} 的行应换算回本地 01:00，实际: {:?}",
            d.first_activity
        );
        assert_eq!(d.last_activity, d.first_activity);
        assert!(d.presence_minutes >= 1);
    }

    /// 联合审查 P1-6 回归：超长间隔按封顶归属而非整段丢弃；credit_tail
    /// 把末次 switch 记到窗口上界（≤ 封顶）。旧实现单 switch 日 3 小时
    /// 连续使用在前台驻留里记 0 秒（`secs > cap → continue`）。
    #[test]
    fn dwell_credits_capped_gap_and_tail() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(crate::db::schema::SCHEMA).unwrap();
        let ins = |conn: &Connection, ts: &str, app: &str| {
            conn.execute(
                "INSERT INTO events (timestamp, event_type, event_action, app_name) \
                 VALUES (?1, 'window', 'switch', ?2)",
                params![ts, app],
            )
            .unwrap();
        };

        // 单 switch：01:00 起 3 小时无后续切换 —— 停摆段按 2h 帽归属
        ins(&conn, "2026-06-15T01:00:00+00:00", "code");
        let out = foreground_dwell_by_app(
            &conn,
            "2026-06-15T00:00:00+00:00",
            "2026-06-15T04:00:00+00:00",
            7200,
            true,
        );
        assert_eq!(
            out,
            vec![("code".into(), 7200)],
            "3h 单 switch 段必须按 2h 帽归属（旧实现整段丢弃记 0）"
        );

        // 停摆段 + 帽内正常段 + 末段 tail：code = 停摆封顶 7200 + 正常 1800；
        // web = 05:30 → 窗口上界 06:00 = 1800（credit_tail）
        let conn2 = Connection::open_in_memory().unwrap();
        conn2.execute_batch(crate::db::schema::SCHEMA).unwrap();
        ins(&conn2, "2026-06-15T01:00:00+00:00", "code");
        ins(&conn2, "2026-06-15T05:00:00+00:00", "code");
        ins(&conn2, "2026-06-15T05:30:00+00:00", "web");
        let out = foreground_dwell_by_app(
            &conn2,
            "2026-06-15T00:00:00+00:00",
            "2026-06-15T06:00:00+00:00",
            7200,
            true,
        );
        assert_eq!(
            out,
            vec![("code".into(), 9000), ("web".into(), 1800)],
            "停摆段封顶 + 正常段全记 + tail 记到上界"
        );
    }

    /// 联合审查（raw 自动化桶）回归：raw 模式纯注入分钟必须计入自动化
    /// 而非人在场——此前 raw 侧只有 human 桶，SendKeys 整夜跑自动化恒 0。
    #[test]
    fn raw_injected_rows_fill_automation_bucket() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(crate::db::schema::SCHEMA).unwrap();
        let day = crate::queries::today_local_str();
        let off = Local::now().format("%:z").to_string();
        let ts = format!("{day}T02:00:00{off}");
        conn.execute(
            "INSERT INTO events (timestamp, event_type, event_action, event_data) \
             VALUES (?1, 'keyboard', 'press', '{\"injected\": true}')",
            params![&ts],
        )
        .unwrap();
        let d = classify_minutes(&conn, &day, 2);
        assert!(
            d.automation_minutes >= 1,
            "注入行必须计入自动化分钟，实际 {}",
            d.automation_minutes
        );
        assert_eq!(d.presence_minutes, 0, "纯注入分钟不得计人在场");
        // first_activity/last_activity 是「人在场」首末活动：纯注入日应为空
        assert_eq!(d.first_activity, None);
    }

    /// 「昨日同时段」截断回归：cap 之后的分钟不计入在场/自动化，且截断口径下
    /// 桥接不越过 cap 点（cap 窗口外的活动不得把窗口尾桥接进来）。
    /// 今天 08:00（本地）有活动、昨天同日 08:00 与 23:00 各有活动：
    /// cap=9h 只应看到 08:00 那一分钟，23:00 的活动不得计入。
    #[test]
    fn capped_classification_excludes_minutes_after_cap() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(crate::db::schema::SCHEMA).unwrap();
        let day = crate::queries::date_offset_str(-1);
        let off = Local::now().format("%:z").to_string();
        for (h, m) in [(8i64, 0i64), (8, 1), (23, 0)] {
            let ts = format!("{day}T{h:02}:{m:02}:00{off}");
            conn.execute(
                "INSERT INTO events (timestamp, event_type, event_action) VALUES (?1, 'keyboard', 'press')",
                params![&ts],
            )
            .unwrap();
        }
        let full = classify_minutes(&conn, &day, 2);
        assert!(
            full.presence_minutes >= 2,
            "全天口径应含 08:00-08:01 与 23:00"
        );

        let capped = classify_minutes_multi_capped(
            &conn,
            std::slice::from_ref(&day.to_string()),
            2,
            Some(9 * 60), // 当日 00:00 起 9 小时
        )
        .into_iter()
        .next()
        .unwrap()
        .1
        .expect("正常日期截断必须可用");
        assert_eq!(
            capped.presence_minutes, 2,
            "cap=9h 只保留 08:00/08:01 两分钟，23:00 不得计入（桥接不得越过截断点）"
        );
        assert_eq!(capped.automation_minutes, 0);
        assert_eq!(capped.last_activity.as_deref(), Some("08:01"));
    }

    /// 审查回归（并行审查 #1）：截断起点解析失败时**不得**静默回退成全天值——
    /// 那会把昨日全天当「昨日同时段」基线，正是 PR #39 要消除的误导无声回归。
    /// 修复后该日返回 None（截断不可用），调用方回退全天口径并如实标注。
    /// 用受控注入模拟解析失败：直接测不动 local_day_range 的失败分支不可达
    /// （真实日期都能解析），因此这里验证契约的另一面——cap=None 时绝不返回
    /// None，且 capped 与 full 完全一致（调用方拿 None 必须显式回退）。
    #[test]
    fn capped_result_is_some_iff_cap_requested_and_day_resolvable() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(crate::db::schema::SCHEMA).unwrap();
        let day = crate::queries::date_offset_str(-1);
        let off = Local::now().format("%:z").to_string();
        let ts = format!("{day}T08:00:00{off}");
        conn.execute(
            "INSERT INTO events (timestamp, event_type, event_action) VALUES (?1, 'keyboard', 'press')",
            params![&ts],
        )
        .unwrap();
        // cap=None：语义=不截断，必须 Some 且与 classify_minutes 等价
        let uncapped =
            &classify_minutes_multi_capped(&conn, std::slice::from_ref(&day.to_string()), 2, None)
                [0];
        assert!(uncapped.1.is_some(), "cap=None 不得产出 None");
        assert_eq!(
            uncapped.1.as_ref().unwrap(),
            &classify_minutes(&conn, &day, 2)
        );
        // cap=Some 且正常日期：必须 Some（截断可用）
        let capped = &classify_minutes_multi_capped(
            &conn,
            std::slice::from_ref(&day.to_string()),
            2,
            Some(9 * 60),
        )[0];
        assert!(capped.1.is_some());
        // 截断可用性契约：返回 None 仅当「要求截断且起点解析失败」，
        // 调用方（dash api_overview）据此回退全天口径并标注，禁止拿 Some 假基线。
    }
}
