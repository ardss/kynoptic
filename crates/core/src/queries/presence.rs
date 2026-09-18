//! 人在场 / 自动化 分钟级分类与桥接 —— 全站唯一权威实现。
//!
//! 权威口径（与 dash overview `minute_classification` 同一 SQL）：
//! - human（人在场）= 真实键鼠：`keys - injected_keys + clicks - injected_clicks > 0`；
//! - auto（自动化）= 注入输入：`injected_keys + injected_clicks > 0`；
//! - 混合分钟两者同时为真，**同时计入** presence 与 automation（mixed 单独返回）；
//! - 相邻在场分钟间隙 <= bridge 分钟按"无输入阅读"桥接（读 settings 的
//!   `presence_bridge_minutes`，0-15，默认 2）；
//! - raw 模式（opt-in 逐键）press/click 无法区分注入，按人算。
//!
//! 消费方：crates/dash（overview / timeline）、crates/cli（presence 子命令）。
//! 禁止在消费方再写第四份口径——需要改动请只改这里。

use chrono::{Local, Timelike, Utc};
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
                    MAX(COALESCE(json_extract(event_data,'$.keys'),0) - COALESCE(json_extract(event_data,'$.injected_keys'),0) + COALESCE(json_extract(event_data,'$.clicks'),0) - COALESCE(json_extract(event_data,'$.injected_clicks'),0)) > 0, \
                    MAX(COALESCE(json_extract(event_data,'$.injected_keys'),0) + COALESCE(json_extract(event_data,'$.injected_clicks'),0)) > 0 \
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
/// 桥接成连续在场段，返回覆盖的分钟总数（经典 afk 模型：间隙按
/// `(diff).min(gap+1)` 补步长，即最多补 gap 个缺失分钟）。
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
pub fn classify_minutes(conn: &Connection, local_day: &str, bridge_min: u32) -> PresenceDay {
    let Some((start, end)) = super::local_day_range(local_day) else {
        return PresenceDay::default();
    };
    let off = super::local_offset_modifier();
    let mut human: Vec<i64> = Vec::new();
    let mut automation: Vec<i64> = Vec::new();
    let mut mixed: i64 = 0;
    let mut first_min: Option<String> = None;
    let mut last_min: Option<String> = None;
    // 本地分钟桶 "YYYY-MM-DD HH:MM" -> 当日第几分钟
    let minute_of_day = |mb: &str| -> Option<i64> {
        chrono::NaiveDateTime::parse_from_str(mb, "%Y-%m-%d %H:%M")
            .ok()
            .map(|t| i64::from(t.hour()) * 60 + i64::from(t.minute()))
    };
    if let Ok(mrows) = minute_classification(conn, &start, &end, &off) {
        for (mb, human_hit, auto_hit) in mrows {
            let mod_ = minute_of_day(&mb);
            if human_hit {
                if let Some(m) = mod_ {
                    human.push(m);
                }
                if first_min.is_none() {
                    first_min = Some(mb.clone());
                }
                last_min = Some(mb.clone());
            }
            if auto_hit {
                if let Some(m) = mod_ {
                    automation.push(m);
                }
                if human_hit {
                    mixed += 1;
                }
            }
        }
    }
    // raw 模式（opt-in 逐键）：press/click 无法区分注入，按人算。
    // 这里 substr 出的是 UTC 分钟串 "YYYY-MM-DDTHH:MM"，需转本地再取当日分钟。
    let minute_of_day_utc = |minute_str: &str| -> Option<i64> {
        use chrono::TimeZone;
        chrono::NaiveDateTime::parse_from_str(minute_str, "%Y-%m-%dT%H:%M")
            .ok()
            .map(|t| {
                let l = Local.from_utc_datetime(&t);
                i64::from(l.hour()) * 60 + i64::from(l.minute())
            })
    };
    if let Ok(mut stmt) = conn.prepare(
        "SELECT DISTINCT substr(timestamp,1,16) FROM events \
         WHERE event_action IN ('press','click') \
           AND timestamp >= ?1 AND timestamp < ?2",
    ) {
        if let Ok(rows) = stmt.query_map(params![&start, &end], |r| r.get::<_, String>(0)) {
            for minute_str in rows.flatten() {
                if let Some(m) = minute_of_day_utc(&minute_str) {
                    human.push(m);
                }
                // 审查 P1：raw 模式（默认粒度）也要维护首末活动——此前只在
                // minute 分类循环里更新，raw 库的 first/last 恒为 null。
                // 本地化成与 minute 模式相同的 "YYYY-MM-DD HH:MM" 格式。
                if let Ok(t) = chrono::NaiveDateTime::parse_from_str(&minute_str, "%Y-%m-%dT%H:%M")
                {
                    use chrono::TimeZone;
                    let l = Local.from_utc_datetime(&t);
                    let local_str = l.format("%Y-%m-%d %H:%M").to_string();
                    if first_min.is_none() {
                        first_min = Some(local_str.clone());
                    }
                    last_min = Some(local_str);
                }
            }
        }
    }
    human.sort_unstable();
    human.dedup();
    automation.sort_unstable();
    automation.dedup();
    let bridge = i64::from(bridge_min.min(15));
    let presence = bridge_count(&human, bridge);
    let fmt_hm = |mb: &str| -> Option<String> {
        chrono::NaiveDateTime::parse_from_str(mb, "%Y-%m-%d %H:%M")
            .ok()
            .map(|t| t.format("%H:%M").to_string())
    };
    PresenceDay {
        presence_minutes: presence,
        automation_minutes: automation.len() as i64,
        mixed_minutes: mixed,
        first_activity: first_min.as_deref().and_then(fmt_hm),
        last_activity: last_min.as_deref().and_then(fmt_hm),
    }
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
