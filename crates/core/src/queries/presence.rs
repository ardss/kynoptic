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
                    MAX(COALESCE(json_extract(event_data,'$.keys'),0) - COALESCE(json_extract(event_data,'$.injected_keys'),0) + COALESCE(json_extract(event_data,'$.clicks'),0) - COALESCE(json_extract(event_data,'$.injected_clicks'),0) + COALESCE(json_extract(event_data,'$.scroll_ticks'),0) - COALESCE(json_extract(event_data,'$.injected_scroll_ticks'),0)) > 0, \
                    MAX(COALESCE(json_extract(event_data,'$.injected_keys'),0) + COALESCE(json_extract(event_data,'$.injected_clicks'),0) + COALESCE(json_extract(event_data,'$.injected_scroll_ticks'),0)) > 0 \
             FROM events \
             WHERE timestamp >= ?2 AND timestamp < ?3 AND event_action = 'input_agg' \
               AND json_valid(event_data) \
             GROUP BY minute_bucket",
        )
        .map_err(|e| e.to_string())?;
    let rows: Vec<(String, bool, bool)> = stmt
        .query_map(params![off, start, end], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, bool>(1)?,
                r.get::<_, bool>(2)?,
            ))
        })
        .map_err(|e| e.to_string())?
        .flatten()
        .collect();
    Ok(rows)
}

/// 桥接计数：排序去重后的分钟序列里，相邻间隙 <= gap 分钟按"无输入阅读"
/// 桥接成连续在场段，返回覆盖的分钟总数。
///
/// 审查复核（day-edge overcount 疑点）：每步计入的是「端点分钟本身 1」+
/// 「桥接补洞 min(diff-1, gap)」，与 `min(diff, gap+1)` 恒等（diff>=1 时
/// `min(diff, gap+1) == 1 + min(diff-1, gap)`），故总增量恒 <= 墙钟跨度 diff，
/// 不存在 diff == gap+1 时多记一分钟的情形；整段总数也恒 <= 首末墙钟跨度。
/// 不得改成裸 `min(gap, diff-1)`——那会把端点分钟本身丢掉，连续分钟序列
/// （diff=1）计数停滞，详见 cli `bridge_count_bridges_small_gaps_only` 测试。
pub fn bridge_count(sorted_minutes: &[i64], gap: i64) -> i64 {
    if sorted_minutes.is_empty() {
        return 0;
    }
    let mut total = 1i64;
    let mut prev = sorted_minutes[0];
    for &m in &sorted_minutes[1..] {
        let step = (m - prev).min(gap + 1);
        total += step.max(1);
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
    let (mut human, automation, mixed) = collect_minute_hits(conn, local_day);
    human.sort_unstable();
    human.dedup();
    let bridge = i64::from(bridge_min.min(15));
    let presence = bridge_count(&human, bridge);
    // 首末人在场：取人侧最小/最大纪元分钟，格式化回本地 "HH:MM"（此前按
    // 行序覆盖在乱序结果集上不可靠，纪元分钟取 min/max 语义精确）。
    let fmt_epoch_hm = |m: i64| -> Option<String> {
        chrono::DateTime::from_timestamp(m * 60, 0)
            .map(|t| t.with_timezone(&Local).format("%H:%M").to_string())
    };
    let first_activity = human.first().copied().and_then(fmt_epoch_hm);
    let last_activity = human.last().copied().and_then(fmt_epoch_hm);
    PresenceDay {
        presence_minutes: presence,
        automation_minutes: automation.len() as i64,
        mixed_minutes: mixed,
        first_activity,
        last_activity,
    }
}

/// 指定本地日的**人在场纪元分钟序列**（升序去重）——全站唯一权威实现。
///
/// 供马拉松等「在场连续性」类异常检测取数：注入输入（injected_keys /
/// injected_clicks / 注入滚轮）不计入、含滚轮滚动，与 [`classify_minutes`]
/// / 总览「今日在场」完全同源——自动化脚本把键盘敲得再响也撑不起「人在场」。
pub fn human_minutes_by_date(conn: &Connection, local_day: &str) -> Vec<i64> {
    let (mut human, _auto, _mixed) = collect_minute_hits(conn, local_day);
    human.sort_unstable();
    human.dedup();
    human
}

/// [`classify_minutes`] / [`human_minutes_by_date`] 的共用取数核心：
/// 返回 (human 纪元分钟, auto 纪元分钟, mixed 分钟数)，均未排序去重。
fn collect_minute_hits(conn: &Connection, local_day: &str) -> (Vec<i64>, Vec<i64>, i64) {
    let Some((start, end)) = super::local_day_range(local_day) else {
        return (Vec::new(), Vec::new(), 0);
    };
    let mut human: Vec<i64> = Vec::new();
    let mut automation: Vec<i64> = Vec::new();
    let mut mixed: i64 = 0;
    // minute 模式：按 UTC 纪元分钟分桶聚合（strftime('%s') 解析 RFC3339 的
    // 时区后缀，两趟重复本地小时得到不同纪元分钟，不再互吞）。
    if let Ok(mut stmt) = conn.prepare(
        "SELECT CAST(strftime('%s', timestamp) AS INTEGER) / 60 AS minute_epoch, \
                MAX(COALESCE(json_extract(event_data,'$.keys'),0) - COALESCE(json_extract(event_data,'$.injected_keys'),0) + COALESCE(json_extract(event_data,'$.clicks'),0) - COALESCE(json_extract(event_data,'$.injected_clicks'),0) + COALESCE(json_extract(event_data,'$.scroll_ticks'),0) - COALESCE(json_extract(event_data,'$.injected_scroll_ticks'),0)) > 0, \
                MAX(COALESCE(json_extract(event_data,'$.injected_keys'),0) + COALESCE(json_extract(event_data,'$.injected_clicks'),0) + COALESCE(json_extract(event_data,'$.injected_scroll_ticks'),0)) > 0 \
         FROM events \
         WHERE timestamp >= ?1 AND timestamp < ?2 AND event_action = 'input_agg' \
           AND json_valid(event_data) \
         GROUP BY minute_epoch",
    ) {
        if let Ok(rows) = stmt.query_map(params![&start, &end], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, bool>(1)?,
                r.get::<_, bool>(2)?,
            ))
        }) {
            for (mepoch, human_hit, auto_hit) in rows.flatten() {
                if human_hit {
                    human.push(mepoch);
                }
                if auto_hit {
                    if human_hit {
                        mixed += 1;
                    }
                    automation.push(mepoch);
                }
            }
        }
    }
    // raw 模式（opt-in 逐键）：排除注入事件——hook 已把 LLKHF_INJECTED /
    // LLMHF_INJECTED 归一化为 event_data 的 "injected" 布尔（见
    // keyboard_hook.rs / mouse_hook.rs 落库字段），注入输入不得计入人在场；
    // 旧数据无该字段时 COALESCE 为 0，仍按人算（与旧行为一致）。
    // 取整条 timestamp 解析（DISTINCT 已去重；回拨日两趟重复本地小时的
    // UTC 串本就不同，不会互吞），折算成纪元分钟。
    // 审查修复：此前 substr 截前 16 字符后强当无时区 UTC 解析——存量/外部
    // 写入的带偏移行（如 '+08:00'）会被整体错算一个时区差。现在优先按
    // RFC3339 整串解析（parse_from_rfc3339 同时接受 'Z' 与 '+08:00' 后缀）
    // 转 UTC；不可解析的旧行退回原 naive-UTC 路径，与写路径
    // normalize_timestamp 同口径。
    if let Ok(mut stmt) = conn.prepare(
        "SELECT DISTINCT timestamp FROM events \
         WHERE event_action IN ('press','click','scroll') \
           AND timestamp >= ?1 AND timestamp < ?2 \
           AND COALESCE(json_extract(event_data,'$.injected'), 0) = 0",
    ) {
        if let Ok(rows) = stmt.query_map(params![&start, &end], |r| r.get::<_, String>(0)) {
            use chrono::TimeZone;
            for ts in rows.flatten() {
                let epoch_min = if let Ok(t) = chrono::DateTime::parse_from_rfc3339(&ts) {
                    t.with_timezone(&Utc).timestamp() / 60
                } else if let Ok(t) = chrono::NaiveDateTime::parse_from_str(&ts, "%Y-%m-%dT%H:%M") {
                    Utc.from_utc_datetime(&t).timestamp() / 60
                } else {
                    continue;
                };
                human.push(epoch_min);
            }
        }
    }
    human.sort_unstable();
    human.dedup();
    automation.sort_unstable();
    automation.dedup();
    (human, automation, mixed)
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
/// `stall_cap_secs`（停摆）不归属；倒序/漂移产生的负间隔按 0 计；
/// `credit_tail` 为 true 时末次 switch 记到窗口上界（不超过封顶）——
/// 报告/MCP 的分段视图需要末段，CLI/overview 汇总不需要。
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
        // 采集停摆（暂停/看门狗杀/关机）的超长间隔不归属；负间隔按 0
        // （时间戳时区偏移漂移时字符串序可能与瞬时序相反）。
        let secs = (pair[1].0 - pair[0].0).num_seconds().max(0);
        if secs > stall_cap_secs {
            continue;
        }
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
}
