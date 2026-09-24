//! 图表数据 + 当前值类查询（dashboard 图表 + analyzer/anomaly 读取）。
//!
//! 异常检测路径（本文件下半部分）优先读 agg_minute / agg_daily **读缓存**
//! （见 [`crate::db::agg`]，派生数据，events 原样保留）；缓存缺失（表不存在 /
//! 该日无缓存行）时回退 events 现算——保证未回填的库行为正确，只是慢。
//!
//! 输入计数类查询统一兼容两种事件形态：raw（press/click 逐行）与
//! input_agg（minute 粒度计数行，见 mod.rs 的 KEYS_ROW_EXPR / CLICKS_ROW_EXPR）。

use rusqlite::{params, Connection};

use super::{
    get_string, string_or_log, DayTotals, MinuteStat, CLICKS_ROW_EXPR, HUMAN_KEYS_ROW_EXPR,
    KEYS_ROW_EXPR,
};
use crate::db::agg;
use crate::db::SqlResult;

// ─── 图表数据（get_events_today/get_apps/get_hourly/get_timeline/get_trend） ──

/// 今日事件 (event_type, event_action) 分布，按计数降序。用于 get_events_today 柱状图。
pub fn action_breakdown_ordered(
    conn: &Connection,
    today: &str,
    tomorrow: &str,
) -> Vec<(String, String, i64)> {
    let mut out = Vec::new();
    let Ok(mut stmt) = conn.prepare(
        "SELECT event_type, event_action, COUNT(*) as cnt FROM events WHERE timestamp >= ?1 AND timestamp < ?2 GROUP BY event_type, event_action ORDER BY cnt DESC"
    ) else {
        return out;
    };
    if let Ok(rows) = stmt.query_map(params![today, tomorrow], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, i64>(2)?,
        ))
    }) {
        out.extend(super::collect_rows_warn(rows, "charts"));
    }
    out
}

/// 今日 Top N 应用排名。app_name 为 NULL 的归为 "(unknown)"。
pub fn top_apps_today(
    conn: &Connection,
    today: &str,
    tomorrow: &str,
    limit: i64,
) -> Vec<(String, i64)> {
    let mut out = Vec::new();
    // 审查 MEDIUM：空白名过滤——只挡空串会让纯空白 app_name（'   '）原样进
    // Top 应用榜（前端渲染为无名列）。TRIM 归一化 + NULLIF 空白归 (unknown)。
    let Ok(mut stmt) = conn.prepare(
        "SELECT COALESCE(NULLIF(TRIM(app_name), ''), '(unknown)'), COUNT(*) as cnt FROM events WHERE timestamp >= ?1 AND timestamp < ?2 AND app_name IS NOT NULL AND TRIM(app_name) != '' GROUP BY app_name ORDER BY cnt DESC LIMIT ?3"
    ) else {
        return out;
    };
    if let Ok(rows) = stmt.query_map(params![today, tomorrow, limit], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
    }) {
        out.extend(super::collect_rows_warn(rows, "charts"));
    }
    out
}

/// 今日 Top N 窗口排名（按 window.switch 出现次数）。
/// 当前 collector 把可读名写入 window_title 而 app_name 多为空，所以这里
/// 直接 GROUP BY window_title，让 AppsView 拿到真实占用前台的窗口列表。
pub fn top_windows_today(
    conn: &Connection,
    today: &str,
    tomorrow: &str,
    limit: i64,
) -> Vec<(String, i64)> {
    let mut out = Vec::new();
    let Ok(mut stmt) = conn.prepare(
        "SELECT window_title, COUNT(*) as cnt FROM events \
         WHERE event_type='window' AND event_action='switch' \
           AND timestamp >= ?1 AND timestamp < ?2 \
           AND window_title IS NOT NULL AND window_title <> '' \
         GROUP BY window_title ORDER BY cnt DESC LIMIT ?3",
    ) else {
        return out;
    };
    if let Ok(rows) = stmt.query_map(params![today, tomorrow, limit], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
    }) {
        out.extend(super::collect_rows_warn(rows, "charts"));
    }
    out
}

/// 今日 window.tab_change 次数。
pub fn tab_change_count_today(conn: &Connection, today: &str, tomorrow: &str) -> i64 {
    conn.query_row(
        "SELECT COUNT(*) FROM events WHERE event_type='window' AND event_action='tab_change' AND timestamp >= ?1 AND timestamp < ?2",
        params![today, tomorrow],
        |r| r.get::<_, i64>(0),
    ).unwrap_or(0)
}

/// 今日每小时事件计数，按小时升序。返回 (hour, count)。
pub fn hourly_counts_today(conn: &Connection, today: &str, tomorrow: &str) -> Vec<(i64, i64)> {
    let mut out = Vec::new();
    // 桶按本地小时取：datetime(timestamp, modifier) 把 UTC 时刻换成本地时刻后取 HH。
    // 事件时刻本地化（审查 HIGH：DST 修复，见 queries::LOCAL_MODIFIER_AT_EVENT）
    let off = super::LOCAL_MODIFIER_AT_EVENT;
    let sql =
        "SELECT CAST(substr(datetime(timestamp, ?1), 12, 2) AS INTEGER) as hour, COUNT(*) as cnt \
         FROM events WHERE timestamp >= ?2 AND timestamp < ?3 GROUP BY hour ORDER BY hour";
    let Ok(mut stmt) = conn.prepare(sql) else {
        return out;
    };
    if let Ok(rows) = stmt.query_map(params![&off, today, tomorrow], |r| {
        Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
    }) {
        out.extend(super::collect_rows_warn(rows, "charts"));
    }
    out
}

/// 今日逐分钟活动计数，按分钟升序。返回 ("HH:MM", count)。
///
/// 桶按**本地分钟**取（同 [`hourly_counts_today`]），否则分钟分布整体错位时区偏移量。
pub fn minute_counts_today(conn: &Connection, today: &str, tomorrow: &str) -> Vec<(String, i64)> {
    let mut out = Vec::new();
    // 事件时刻本地化（审查 HIGH：DST 修复，见 queries::LOCAL_MODIFIER_AT_EVENT）
    let off = super::LOCAL_MODIFIER_AT_EVENT;
    let sql = "SELECT substr(datetime(timestamp, ?1), 12, 5) as minute, COUNT(*) as cnt \
         FROM events WHERE timestamp >= ?2 AND timestamp < ?3 GROUP BY minute ORDER BY minute";
    let Ok(mut stmt) = conn.prepare(sql) else {
        return out;
    };
    if let Ok(rows) = stmt.query_map(params![&off, today, tomorrow], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
    }) {
        out.extend(super::collect_rows_warn(rows, "charts"));
    }
    out
}

/// 自 since 起每日事件计数，按**本地日期**分组。返回 ("YYYY-MM-DD", count)。
///
/// 用于 get_trend（近 7 天趋势）。桶按本地日期取，否则趋势图会把本地凌晨数据归到前一日。
/// WHERE 走 timestamp 范围索引，GROUP BY 用 datetime(timestamp, modifier) 投影。
pub fn daily_counts_since(conn: &Connection, since: &str) -> Vec<(String, i64)> {
    let mut out = Vec::new();
    // 事件时刻本地化（审查 HIGH：DST 修复，见 queries::LOCAL_MODIFIER_AT_EVENT）
    let off = super::LOCAL_MODIFIER_AT_EVENT;
    // 审查 MEDIUM：上界排除 timestamp > 当前 UTC 的未来行——时钟拨快期间
    // 写入的脏数据不得画出未来柱（与 dash 侧 reject_future_date 同口径）。
    let now = chrono::Utc::now().to_rfc3339();
    let sql = "SELECT substr(datetime(timestamp, ?1), 1, 10) as date, COUNT(*) as cnt \
         FROM events WHERE timestamp >= ?2 AND timestamp < ?3 GROUP BY date";
    let Ok(mut stmt) = conn.prepare(sql) else {
        return out;
    };
    if let Ok(rows) = stmt.query_map(params![&off, since, &now], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
    }) {
        out.extend(super::collect_rows_warn(rows, "charts"));
    }
    out
}

/// 今日所有 keyboard/press 事件的 event_data（原始 JSON 字符串）。
/// 用于 get_heatmap 在内存中聚合按键频率与快捷键组合。
pub fn keyboard_press_data_today(conn: &Connection, today: &str, tomorrow: &str) -> Vec<String> {
    let mut out = Vec::new();
    let Ok(mut stmt) = conn.prepare(
        "SELECT event_data FROM events WHERE event_type='keyboard' AND event_action='press' AND timestamp >= ?1 AND timestamp < ?2 AND event_data IS NOT NULL"
    ) else {
        return out;
    };
    if let Ok(rows) = stmt.query_map(params![today, tomorrow], |r| r.get::<_, String>(0)) {
        out.extend(super::collect_rows_warn(rows, "charts"));
    }
    out
}

/// 今日全部 mouse 事件的 (event_action, event_data) —— 用于鼠标热力聚合。
/// 一次取 click/release/move/scroll 全部动作的原始 JSON（含 x/y 坐标），
/// 在内存里按动作分桶 + 坐标网格化（算法见 [`crate::heatmap::aggregate_mouse`]）。
pub fn mouse_event_data_today(
    conn: &Connection,
    today: &str,
    tomorrow: &str,
) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let Ok(mut stmt) = conn.prepare(
        "SELECT event_action, event_data FROM events WHERE event_type='mouse' AND timestamp >= ?1 AND timestamp < ?2 AND event_data IS NOT NULL"
    ) else {
        return out;
    };
    if let Ok(rows) = stmt.query_map(params![today, tomorrow], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    }) {
        out.extend(super::collect_rows_warn(rows, "charts"));
    }
    out
}

// ─── 当前值类查询（前台窗口 / 最近输入） ─────────────────────────────────────

pub fn current_app(conn: &Connection, today: &str, tomorrow: &str) -> String {
    string_or_log(
        conn.query_row(
            "SELECT COALESCE(app_name, '') FROM events WHERE event_type='window' AND timestamp >= ?1 AND timestamp < ?2 AND app_name IS NOT NULL ORDER BY timestamp DESC LIMIT 1",
            params![today, tomorrow],
            get_string,
        ),
        "current_app",
    )
}

pub fn current_window_title(conn: &Connection, today: &str, tomorrow: &str) -> String {
    string_or_log(
        conn.query_row(
            "SELECT COALESCE(window_title, '') FROM events WHERE event_type='window' AND timestamp >= ?1 AND timestamp < ?2 AND window_title IS NOT NULL ORDER BY timestamp DESC LIMIT 1",
            params![today, tomorrow],
            get_string,
        ),
        "current_window_title",
    )
}

/// 单次查询取最新的前台应用名 + 窗口标题，替代 current_app + current_window_title 两次独立查询。
/// 返回 (app_name, window_title)，无匹配时均为空串。
pub fn current_app_title(conn: &Connection, today: &str, tomorrow: &str) -> (String, String) {
    match conn.query_row(
        "SELECT COALESCE(app_name, ''), COALESCE(window_title, '') FROM events WHERE event_type='window' AND timestamp >= ?1 AND timestamp < ?2 AND (app_name IS NOT NULL OR window_title IS NOT NULL) ORDER BY timestamp DESC LIMIT 1",
        params![today, tomorrow],
        |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
    ) {
        Ok(v) => v,
        Err(rusqlite::Error::QueryReturnedNoRows) => (String::new(), String::new()),
        Err(e) => {
            log::warn!("查询失败(current_app_title): {e}");
            (String::new(), String::new())
        }
    }
}

/// 全局最新前台应用（无日期窗口）。用于 snapshot 等需要 all-time 最新窗口事件的场景。
/// 与 [`current_app`] 的差异：后者限定今日窗口，本函数取全局最新。
pub fn current_app_all_time(conn: &Connection) -> String {
    string_or_log(
        conn.query_row(
            "SELECT COALESCE(app_name,'') FROM events WHERE event_type='window' AND app_name IS NOT NULL ORDER BY timestamp DESC LIMIT 1",
            [],
            get_string,
        ),
        "current_app_all_time",
    )
}

/// 最早事件时间戳（截断到秒）。无数据返回空串。
pub fn earliest_event_ts(conn: &Connection) -> String {
    string_or_log(
        conn.query_row(
            "SELECT substr(timestamp,1,19) FROM events ORDER BY timestamp ASC LIMIT 1",
            [],
            get_string,
        ),
        "earliest_event_ts",
    )
}

/// 最新事件时间戳（截断到秒）。无数据返回空串。
pub fn latest_event_ts(conn: &Connection) -> String {
    string_or_log(
        conn.query_row(
            "SELECT substr(timestamp,1,19) FROM events ORDER BY timestamp DESC LIMIT 1",
            [],
            get_string,
        ),
        "latest_event_ts",
    )
}

pub fn last_input_timestamp(conn: &Connection) -> Option<String> {
    conn.query_row(
        "SELECT MAX(timestamp) FROM events WHERE event_type IN ('keyboard','mouse') AND event_action IN ('press','click','scroll','move','input_agg')",
        [],
        |r| r.get(0),
    )
    .ok()
    .flatten()
}

// ─── analyzer / anomaly 数据读取 ─────────────────────────────────────────────

/// 一分钟内的活动统计：keys / clicks / 窗口切换数。
///
/// 由 [`crate::analyzer`] 的专注段、APM 序列、碎片化算法消费——此前这些业务
/// 模块各自手写 `SELECT substr(timestamp, 1, 16) ... GROUP BY minute`。现在统一
/// 在本数据访问层读取，业务层只拿纯数据做规则计算（参见 [`MinuteStat`]）。
///
/// 注：与 [`minute_counts_today`] 不同——后者只统计事件数,本函数按 type/action
/// 分别聚合 keys/clicks/switches,供专注段/APM 算法使用。
pub fn minute_stats_by_date(conn: &Connection, date: &str) -> Vec<MinuteStat> {
    let mut out = Vec::new();
    // 优先读 agg_minute 读缓存（异常检测/分析路径提速）；无缓存回退 events 现算。
    if agg::has_minute_for_date(conn, date) {
        let Ok(mut stmt) = conn.prepare(
            "SELECT hour, minute, \
                    CAST(MAX(CASE WHEN bucket_id='input_keys' THEN COALESCE(sum_value,0) ELSE 0 END) AS INTEGER), \
                    CAST(MAX(CASE WHEN bucket_id='input_clicks' THEN COALESCE(sum_value,0) ELSE 0 END) AS INTEGER), \
                    CAST(MAX(CASE WHEN bucket_id='window_switches' THEN COALESCE(sum_value,0) ELSE 0 END) AS INTEGER) \
             FROM agg_minute \
             WHERE date = ?1 AND bucket_id IN ('input_keys','input_clicks','window_switches') \
             GROUP BY hour, minute ORDER BY hour, minute",
        ) else {
            return out;
        };
        if let Ok(rows) = stmt.query_map(params![date], |r| {
            let hour: i64 = r.get(0)?;
            let minute: i64 = r.get(1)?;
            Ok(MinuteStat {
                minute: format!("{date}T{:02}:{:02}", hour, minute),
                keys: r.get::<_, i64>(2)?,
                clicks: r.get::<_, i64>(3)?,
                switches: r.get::<_, i64>(4)?,
            })
        }) {
            out.extend(super::collect_rows_warn(rows, "charts"));
        }
        return out;
    }
    // 按用户本地时区切日（与 today_range 同源）。解析失败回退 UTC 前缀匹配。
    let (start, end) = match super::local_day_range(date) {
        Some(r) => r,
        None => (date.to_string(), format!("{date}\u{7f}")),
    };
    // 分钟桶与 agg 路径同构：本地钟面 "YYYY-MM-DDTHH:MM"（datetime 输出为空格
    // 分隔，replace 成 "T"）。此前回退路径直接 substr UTC 原串，异常卡上时区错位。
    // 事件时刻本地化（审查 HIGH：DST 修复，见 queries::LOCAL_MODIFIER_AT_EVENT）
    let off = super::LOCAL_MODIFIER_AT_EVENT;
    let Ok(mut stmt) = conn.prepare(&format!(
        "SELECT replace(substr(datetime(timestamp, ?3), 1, 16), ' ', 'T') AS minute, \
                SUM({KEYS_ROW_EXPR}) AS keys, \
                SUM({CLICKS_ROW_EXPR}) AS clicks, \
                SUM(CASE WHEN event_type='window' AND event_action='switch' THEN 1 ELSE 0 END) AS switches \
         FROM events \
         WHERE timestamp >= ?1 AND timestamp < ?2 \
           AND event_type IN ('keyboard','mouse','window') \
         GROUP BY minute \
         ORDER BY minute",
    )) else {
        return out;
    };
    let Ok(rows) = stmt.query_map(params![start, end, off], |r| {
        Ok(MinuteStat {
            minute: r.get::<_, String>(0)?,
            keys: r.get::<_, Option<i64>>(1)?.unwrap_or(0),
            clicks: r.get::<_, Option<i64>>(2)?.unwrap_or(0),
            switches: r.get::<_, Option<i64>>(3)?.unwrap_or(0),
        })
    }) else {
        return out;
    };
    out.extend(super::collect_rows_warn(rows, "charts.minute_stats"));
    out
}

/// 当日活跃分钟列表（ISO "YYYY-MM-DDTHH:MM"），按时间升序。
///
/// 口径（统一 2026-09）：**剔除 move-only 分钟**——仅当该分钟有 keys 或
/// clicks 才算活跃（脚本级纯鼠标移动不能伪造活跃）。
///
/// **注意（2026-09 审查）**：本函数是「键鼠输入活跃」口径（含自动化注入
/// 输入），仅供 [`crate::analyzer::fragmentation_score`] 等输入类分析使用；
/// 「人在场」类判定（如马拉松检测）必须用 [`super::human_minutes_by_date`]
/// （剔注入、含滚轮，与总览在场同源），禁止再把本函数当「在场」数据源。
pub fn active_minutes_by_date(conn: &Connection, date: &str) -> Vec<String> {
    let mut out = Vec::new();
    // 优先读 agg_minute 读缓存（有 keys/clicks 桶行即视为活跃分钟，不含 move-only）
    if agg::has_minute_for_date(conn, date) {
        let Ok(mut stmt) = conn.prepare(
            "SELECT hour, minute FROM agg_minute \
             WHERE date = ?1 \
               AND bucket_id IN ('input_keys','input_clicks') \
             GROUP BY hour, minute ORDER BY hour, minute",
        ) else {
            return out;
        };
        if let Ok(rows) = stmt.query_map(params![date], |r| {
            let hour: i64 = r.get(0)?;
            let minute: i64 = r.get(1)?;
            Ok(format!("{date}T{hour:02}:{minute:02}"))
        }) {
            out.extend(super::collect_rows_warn(rows, "charts"));
        }
        return out;
    }
    let (start, end) = match super::local_day_range(date) {
        Some(r) => r,
        None => (date.to_string(), format!("{date}\u{7f}")),
    };
    // 口径与 agg 路径对齐（交叉审查 P1）：mouse input_agg 行可能只有 moves
    // （纯移动无点击），不能凭"存在 input_agg 行"判活跃——必须按
    // KEYS_ROW_EXPR + CLICKS_ROW_EXPR > 0 判定（keys/clicks 才算活跃分钟）。
    // 分钟串同样与 agg 路径同构：本地钟面 "YYYY-MM-DDTHH:MM"（datetime 输出
    // 为空格分隔，replace 成 "T"；此前回退路径返回 UTC 原串，异常卡上时区错位）。
    // 事件时刻本地化（审查 HIGH：DST 修复，见 queries::LOCAL_MODIFIER_AT_EVENT）
    let off = super::LOCAL_MODIFIER_AT_EVENT;
    let Ok(mut stmt) = conn.prepare(&format!(
        "SELECT DISTINCT replace(substr(datetime(timestamp, ?3), 1, 16), ' ', 'T') AS minute \
         FROM events \
         WHERE timestamp >= ?1 AND timestamp < ?2 \
           AND event_type IN ('keyboard','mouse') \
           AND ({KEYS_ROW_EXPR} + {CLICKS_ROW_EXPR}) > 0 \
         ORDER BY minute",
    )) else {
        return out;
    };
    let Ok(rows) = stmt.query_map(params![start, end, off], |r| r.get::<_, String>(0)) else {
        return out;
    };
    out.extend(super::collect_rows_warn(rows, "charts.active_minutes"));
    out
}

/// 当日 keys/clicks/active_minutes 三合一（`analyze_day` 编排层用）。
///
/// 此前 [`crate::analyzer::analyze_day`] 手写一条带子查询的 SELECT；现在下沉到
/// 数据访问层，业务层只做 apm_avg 计算。
///
/// 错误契约（库损坏静默零值修复 2026-09）：查询失败必须上抛而非回退零值——
/// 聚合 SELECT 恒有一行，Err 只可能是库损坏/IO 故障（如 "database disk image
/// is malformed"），吞掉后调用方会把"库坏了"当成"今天什么都没干"（此前两处
/// `else return DayTotals::default()` 的实际后果）。
pub fn day_totals(conn: &Connection, date: &str) -> SqlResult<DayTotals> {
    // 优先读 agg_minute 读缓存
    if agg::has_minute_for_date(conn, date) {
        let (keys, clicks, active_minutes) = conn.query_row(
            "SELECT \
                CAST(COALESCE(SUM(CASE WHEN bucket_id='input_keys' THEN COALESCE(sum_value,0) ELSE 0 END), 0) AS INTEGER), \
                CAST(COALESCE(SUM(CASE WHEN bucket_id='input_clicks' THEN COALESCE(sum_value,0) ELSE 0 END), 0) AS INTEGER), \
                -- 口径（统一 2026-09，交叉审查 P1）：剔除 move-only 分钟，
                -- 与 active_minutes_by_date / daily_agg.recompute_day /
                -- active_minutes_today 同一口径（keys/clicks 才算活跃）。
                COUNT(DISTINCT CASE WHEN bucket_id IN ('input_keys','input_clicks') \
                                    THEN printf('%02d:%02d', hour, minute) END) \
             FROM agg_minute WHERE date = ?1",
            params![date],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?)),
        )?;
        return Ok(DayTotals {
            keys,
            clicks,
            active_minutes,
        });
    }
    let (start, end) = match super::local_day_range(date) {
        Some(r) => r,
        None => (date.to_string(), format!("{date}\u{7f}")),
    };
    let (keys, clicks, active_minutes) = conn.query_row(
        // 口径（统一 2026-09，交叉审查 P1）：move-only 分钟不算活跃，
        // 与 active_minutes_by_date / daily_agg / active_minutes_today 同口径。
        &format!(
            "SELECT \
                COALESCE(SUM({KEYS_ROW_EXPR}), 0), \
                COALESCE(SUM({CLICKS_ROW_EXPR}), 0), \
                COUNT(DISTINCT CASE WHEN {KEYS_ROW_EXPR} + {CLICKS_ROW_EXPR} > 0 \
                                    THEN substr(timestamp, 1, 16) END) \
             FROM events \
             WHERE timestamp >= ?1 AND timestamp < ?2 \
               AND event_type IN ('keyboard','mouse')"
        ),
        params![start, end],
        |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
            ))
        },
    )?;
    Ok(DayTotals {
        keys,
        clicks,
        active_minutes,
    })
}

/// 在 `[start, end)` 分钟区间内取最频繁的 (app_name, window_title)。
///
/// 供 [`crate::analyzer`] 为专注段补全 app/window 标签。`end` 是 "YYYY-MM-DDTHH:MM"
/// 形式，本函数把它换算成**半开**的下一分钟上界。
///
/// 审查 MEDIUM：旧实现补 `':59'` 与真实 timestamp 的 `.nnnnnn+00:00` 后缀做
/// 字典序比较恒为假，该分钟第 59 秒内的事件永远不参与标注；建议的
/// `datetime(end,'+60 seconds')` 也无效（datetime 输出空格分隔，与 'T' 分隔
/// 的 timestamp 比较同样恒为假）。改为在 Rust 侧 +1 分钟、用 strftime 同构的
/// 'T' 分隔格式做 `timestamp < 上界`（解析失败兜底补 ':59.999999' 后缀）。
pub fn top_app_window_in_range(
    conn: &Connection,
    start: &str,
    end: &str,
) -> Option<(String, String)> {
    let upper = chrono::NaiveDateTime::parse_from_str(&format!("{end}:00"), "%Y-%m-%dT%H:%M:%S")
        .ok()
        .and_then(|t| t.checked_add_signed(chrono::Duration::minutes(1)))
        .map(|t| t.format("%Y-%m-%dT%H:%M:%S").to_string())
        .unwrap_or_else(|| format!("{end}:59.999999"));
    conn.query_row(
        "SELECT app_name, window_title \
         FROM events \
         WHERE timestamp >= ?1 AND timestamp < ?2 \
           AND event_type IN ('window', 'keyboard', 'mouse') \
           AND COALESCE(app_name, '') <> '' \
         GROUP BY app_name, window_title \
         ORDER BY COUNT(*) DESC \
         LIMIT 1",
        params![start, &upper],
        |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
    )
    .ok()
}

// ─── 异常检测数据读取（供 [`crate::anomaly`] 消费） ──────────────────────────

/// 当日深夜窗口 `[hour_start, hour_end)` 内（跨午夜：本地小时 ≥ hour_start
/// **或** < hour_end，如 23:00-06:00）的键盘按键数。
///
/// 供 [`crate::anomaly`] 的深夜活动检测——此前该业务模块手写
/// `COUNT(*) WHERE CAST(substr(timestamp,12,2)) >= ?`。
///
/// 口径（统一 2026-09）：调用方传 `(23, 6)`，深夜 = 本地 23:00-06:00，与
/// insights 深夜卡同一窗口；跨午夜部分按同一本地日的 hour<6 计。
///
/// 性能注（perf-query 1M 事件库 2026-09 实测）：日期过滤必须用
/// `timestamp >= ?1 AND timestamp < ?2` 的可走索引（idx_events_timestamp）
/// 区间谓词；`substr(timestamp,1,10)=?` 对整列求值无法走索引，1M 行时每次
/// 调用退化为全表扫描（~300ms+），get_anomalies(7d) 会累计到秒级。
pub fn late_night_key_count(conn: &Connection, date: &str, hour_start: i64, hour_end: i64) -> i64 {
    // 优先读 agg_minute 读缓存（O(当日聚合行数)），缺失回退 events 现算。
    // 人侧口径（审查修复 2026-09）：扣减注入拆桶 `input_keys_injected`，
    // 纯注入分钟不得计成"深夜按键"；旧缓存无该桶时按 0 注入处理（等价旧行为）。
    if agg::has_minute_for_date(conn, date) {
        return conn
            .query_row(
                "SELECT CAST(COALESCE(SUM(MAX(COALESCE(k,0) - COALESCE(j,0), 0)), 0) AS INTEGER) FROM (\
                     SELECT hour, minute, \
                            SUM(CASE WHEN bucket_id = 'input_keys' THEN COALESCE(sum_value,0) ELSE 0 END) AS k, \
                            SUM(CASE WHEN bucket_id = 'input_keys_injected' THEN COALESCE(sum_value,0) ELSE 0 END) AS j \
                     FROM agg_minute \
                     WHERE date = ?1 AND (hour >= ?2 OR hour < ?3) \
                       AND bucket_id IN ('input_keys','input_keys_injected') \
                     GROUP BY hour, minute)",
                params![date, hour_start, hour_end],
                |r| r.get::<_, i64>(0),
            )
            .unwrap_or(0);
    }
    // 事件时刻本地化（审查 HIGH：DST 修复，见 queries::LOCAL_MODIFIER_AT_EVENT）
    let off = super::LOCAL_MODIFIER_AT_EVENT;
    match super::local_day_range(date) {
        Some((start, end)) => {
            late_night_key_count_in_range(conn, &start, &end, hour_start, hour_end, off)
        }
        None => {
            let end = format!("{date}\u{7f}");
            late_night_key_count_in_range(conn, date, &end, hour_start, hour_end, off)
        }
    }
}

/// [`late_night_key_count`] 的机器无关核心：显式接收 UTC `[start, end)` 边界与
/// 本地偏移修饰符（供单测注入固定 UTC+8 语义）。
///
/// **时区语义（fix 2026-09）**：小时过滤必须先把 UTC timestamp 换算成**本地**时刻
/// （`datetime(timestamp, ?5)`）再 `substr` 取 HH——此前直接 `substr(timestamp,12,2)`
/// 取的是 UTC 小时，UTC+8 下本地 07:30 会被误判为深夜（UTC 23 点），真深夜 23:30
/// （UTC 15 点）反而漏报。与 [`hourly_counts_today`] 的本地小时桶同一模式。
/// **跨午夜（统一 2026-09）**：窗口为 `hour >= hour_start OR hour < hour_end`
/// （如 23 点后或 6 点前都算深夜）。
pub(crate) fn late_night_key_count_in_range(
    conn: &Connection,
    start: &str,
    end: &str,
    hour_start: i64,
    hour_end: i64,
    off_modifier: &str,
) -> i64 {
    conn.query_row(
        &format!(
            // 人侧口径（审查修复 2026-09）：HUMAN_KEYS_ROW_EXPR 扣注入，
            // 自动化注入按键不算"深夜按键"（与 presence/insights 剔注入口径一致）。
            "SELECT COALESCE(SUM({HUMAN_KEYS_ROW_EXPR}), 0) FROM events \
             WHERE timestamp >= ?1 AND timestamp < ?2 \
               AND (CAST(substr(datetime(timestamp, ?5), 12, 2) AS INTEGER) >= ?3 \
                    OR CAST(substr(datetime(timestamp, ?5), 12, 2) AS INTEGER) < ?4) \
               AND event_type = 'keyboard' AND event_action IN ('press','input_agg')"
        ),
        params![start, end, hour_start, hour_end, off_modifier],
        |r| r.get::<_, i64>(0),
    )
    .unwrap_or(0)
}

/// `date` 之前所有 `daily_agg` 行的 apm_avg 均值（无数据返回 0.0）。
///
/// 供 [`crate::anomaly`] 的 APM 突增检测作为历史基线。
pub fn daily_agg_avg_apm_before(conn: &Connection, date: &str) -> f64 {
    // 审查：apm_avg <= 0 的行（无输入的"零天"）参与平均会把基线稀释、
    // 突增漏报——只对真正有输入的历史日均取平均；全部为 0 时 AVG 返回 NULL，
    // COALESCE 落回 0.0（无基线不报警的既有语义不变）。
    conn.query_row(
        "SELECT COALESCE(AVG(apm_avg), 0.0) FROM daily_agg WHERE date < ?1 AND apm_avg > 0",
        params![date],
        |r| r.get::<_, f64>(0),
    )
    .unwrap_or(0.0)
}

/// 当日 top-5 高强度分钟（人侧 keys+clicks 合计 ≥ `min_count`），按强度倒序。
///
/// 返回 (minute, count)，供 [`crate::anomaly`] 的 APM 突增检测消费。
/// 人侧口径（审查修复 2026-09）：扣减注入（自动化）输入——纯注入分钟
/// （{"keys":500,"injected_keys":500}）不再被当成行为突增报警。
pub fn top_burst_minutes(conn: &Connection, date: &str, min_count: i64) -> Vec<(String, i64)> {
    let mut out = Vec::new();
    // 优先读 agg_minute 读缓存：每分钟人侧 keys+clicks 合计（扣注入拆桶）
    if agg::has_minute_for_date(conn, date) {
        let Ok(mut stmt) = conn.prepare(
            "SELECT hour, minute, CAST(MAX(COALESCE(k,0) - COALESCE(j,0), 0) \
                 + MAX(COALESCE(c,0) - COALESCE(ic,0), 0) AS INTEGER) AS n \
             FROM (\
                 SELECT hour, minute, \
                        SUM(CASE WHEN bucket_id='input_keys' THEN COALESCE(sum_value,0) ELSE 0 END) AS k, \
                        SUM(CASE WHEN bucket_id='input_keys_injected' THEN COALESCE(sum_value,0) ELSE 0 END) AS j, \
                        SUM(CASE WHEN bucket_id='input_clicks' THEN COALESCE(sum_value,0) ELSE 0 END) AS c, \
                        SUM(CASE WHEN bucket_id='input_clicks_injected' THEN COALESCE(sum_value,0) ELSE 0 END) AS ic \
                 FROM agg_minute \
                 WHERE date = ?1 \
                   AND bucket_id IN ('input_keys','input_keys_injected','input_clicks','input_clicks_injected') \
                 GROUP BY hour, minute) \
             GROUP BY hour, minute HAVING n >= ?2 \
             ORDER BY n DESC LIMIT 5",
        ) else {
            return out;
        };
        if let Ok(rows) = stmt.query_map(params![date, min_count], |r| {
            let hour: i64 = r.get(0)?;
            let minute: i64 = r.get(1)?;
            let n: i64 = r.get(2)?;
            Ok((format!("{date}T{hour:02}:{minute:02}"), n))
        }) {
            out.extend(super::collect_rows_warn(rows, "charts"));
        }
        return out;
    }
    // 可走索引的日期区间谓词（见 late_night_key_count 性能注）
    let (start, end) = match super::local_day_range(date) {
        Some(r) => r,
        None => (date.to_string(), format!("{date}\u{7f}")),
    };
    // 事件时刻本地化（审查 HIGH：DST 修复，见 queries::LOCAL_MODIFIER_AT_EVENT）
    let off = super::LOCAL_MODIFIER_AT_EVENT;
    top_burst_minutes_in_range(conn, &start, &end, min_count, off)
}

/// [`top_burst_minutes`] 的机器无关核心：显式接收 UTC `[start, end)` 边界与
/// 本地偏移修饰符（供单测注入固定 UTC+8 语义）。
///
/// **时区语义（交叉审查 P2）**：分钟串必须先把 UTC timestamp 换算成**本地**时刻
/// （`datetime(timestamp, ?4)`）再截断——此前直接 `substr(timestamp,1,16)` 返回
/// UTC 原串，本地 12:00 的突增在异常卡上显示为 UTC 04:00。与 agg 缓存路径
/// （bucket 即本地分钟）及 dash 其他显示一致。
pub(crate) fn top_burst_minutes_in_range(
    conn: &Connection,
    start: &str,
    end: &str,
    min_count: i64,
    off_modifier: &str,
) -> Vec<(String, i64)> {
    let mut out = Vec::new();
    let Ok(mut stmt) = conn.prepare(&format!(
        // datetime() 输出为 "YYYY-MM-DD HH:MM"（空格分隔），replace 成 "T" 与
        // agg 缓存路径的分钟串格式保持一致（"YYYY-MM-DDTHH:MM"）。
        // 人侧口径（审查修复 2026-09）：HUMAN_*_ROW_EXPR 扣注入。
        "SELECT replace(substr(datetime(timestamp, ?4), 1, 16), ' ', 'T'), \
         SUM({HUMAN_KEYS_ROW_EXPR} + {HUMAN_CLICKS_ROW_EXPR}) AS n \
         FROM events \
         WHERE timestamp >= ?1 AND timestamp < ?2 \
           AND event_type IN ('keyboard', 'mouse') \
           AND event_action IN ('press', 'click', 'input_agg') \
         GROUP BY 1 \
         HAVING n >= ?3 \
         ORDER BY n DESC \
         LIMIT 5",
        HUMAN_KEYS_ROW_EXPR = super::HUMAN_KEYS_ROW_EXPR,
        HUMAN_CLICKS_ROW_EXPR = super::HUMAN_CLICKS_ROW_EXPR,
    )) else {
        return out;
    };
    let Ok(rows) = stmt.query_map(params![start, end, min_count, off_modifier], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
    }) else {
        return out;
    };
    out.extend(super::collect_rows_warn(rows, "charts.top_burst"));
    out
}

/// 当日含注入（自动化）输入的分钟集合（本地 "YYYY-MM-DDTHH:MM"）。
///
/// 供 [`crate::anomaly`] 的 APM 突增消息对混合分钟标注"含自动化注入"，
/// 避免无人值守分钟被渲染成纯行为异常。
pub fn injected_input_minutes_by_date(
    conn: &Connection,
    date: &str,
) -> std::collections::HashSet<String> {
    let mut out = std::collections::HashSet::new();
    let off = super::LOCAL_MODIFIER_AT_EVENT;
    // agg 缓存路径：注入拆桶行即注入分钟
    if agg::has_minute_for_date(conn, date) {
        let Ok(mut stmt) = conn.prepare(
            "SELECT hour, minute FROM agg_minute \
             WHERE date = ?1 \
               AND bucket_id IN ('input_keys_injected','input_clicks_injected') \
               AND COALESCE(sum_value, 0) > 0 \
             GROUP BY hour, minute",
        ) else {
            return out;
        };
        if let Ok(rows) = stmt.query_map(params![date], |r| {
            let hour: i64 = r.get(0)?;
            let minute: i64 = r.get(1)?;
            Ok(format!("{date}T{hour:02}:{minute:02}"))
        }) {
            out.extend(super::collect_rows_warn(rows, "charts.injected_minutes"));
        }
        return out;
    }
    // events 回退：raw 行认 $.injected，input_agg 行认 $.injected_keys/clicks
    let Some((start, end)) = super::local_day_range(date) else {
        return out;
    };
    let Ok(mut stmt) = conn.prepare(&format!(
        "SELECT DISTINCT replace(substr(datetime(timestamp, ?3), 1, 16), ' ', 'T') \
         FROM events \
         WHERE timestamp >= ?1 AND timestamp < ?2 \
           AND event_type IN ('keyboard','mouse') \
           AND event_action IN ('press','click','input_agg') \
           AND ({INJ_KEYS} + {INJ_CLICKS}) > 0",
        INJ_KEYS = super::INJECTED_KEYS_ROW_EXPR,
        INJ_CLICKS = super::INJECTED_CLICKS_ROW_EXPR,
    )) else {
        return out;
    };
    if let Ok(rows) = stmt.query_map(params![start, end, off], |r| r.get::<_, String>(0)) {
        out.extend(super::collect_rows_warn(rows, "charts.injected_minutes"));
    }
    out
}

/// 当日 top-N 应用（按事件数倒序），过滤 keyboard/mouse/window 事件且 app_name 非空。
///
/// 供 [`crate::anomaly`] 的新应用突增检测消费。
pub fn top_apps_by_event_types(conn: &Connection, date: &str, limit: i64) -> Vec<(String, i64)> {
    let mut out = Vec::new();
    // 优先读 agg_daily per-app 读缓存（该日有缓存行时）
    if agg::has_app_daily(conn, Some(date)) {
        let Ok(mut stmt) = conn.prepare(
            "SELECT substr(bucket_id, 5), COALESCE(count_value, 0) \
             FROM agg_daily \
             WHERE date = ?1 AND bucket_id LIKE 'app:%' \
             ORDER BY count_value DESC LIMIT ?2",
        ) else {
            return out;
        };
        if let Ok(rows) = stmt.query_map(params![date, limit], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
        }) {
            out.extend(super::collect_rows_warn(rows, "charts"));
        }
        return out;
    }
    // 可走索引的日期区间谓词（见 late_night_key_count 性能注）
    let (start, end) = match super::local_day_range(date) {
        Some(r) => r,
        None => (date.to_string(), format!("{date}\u{7f}")),
    };
    // 审查 HIGH：input_agg 行经 .app("", "") 落库时 app_name 为空串（非 NULL），
    // 只过滤 IS NOT NULL 会让空名幽灵行成为 top-1，与上方 agg 缓存路径
    // （rebuild/backfill 均过滤 app_name != ''）结果分叉——与 top_apps_today
    // 同款补上非空串过滤。
    let Ok(mut stmt) = conn.prepare(
        "SELECT IFNULL(app_name, '(unknown)') AS app, COUNT(*) AS n \
         FROM events \
         WHERE timestamp >= ?1 AND timestamp < ?2 \
           AND app_name IS NOT NULL AND app_name != '' \
           AND event_type IN ('keyboard', 'mouse', 'window') \
         GROUP BY app \
         ORDER BY n DESC \
         LIMIT ?3",
    ) else {
        return out;
    };
    let Ok(rows) = stmt.query_map(params![start, end, limit], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
    }) else {
        return out;
    };
    for row in rows.flatten() {
        out.push(row);
    }
    out
}

/// 某应用在 `before_date` 之前的事件总数与出现天数。
///
/// 返回 (hist_total, hist_days)，供 [`crate::anomaly`] 的新应用突增检测计算日均。
///
/// 性能：优先读 agg_daily per-app 读缓存（全历史 per-app 统计从 O(该应用全部
/// 历史行) 降为 O(该应用出现天数)——perf-query 1M 行合成库上这是 get_anomalies
/// 的最后一个秒级瓶颈）；无任何 app 缓存时回退 events 现算。
pub fn app_history_totals(conn: &Connection, app: &str, before_date: &str) -> (i64, i64) {
    if agg::has_app_daily(conn, None) {
        return conn
            .query_row(
                "SELECT COALESCE(SUM(count_value), 0), COUNT(*) \
                 FROM agg_daily \
                 WHERE bucket_id = 'app:' || ?1 AND date < ?2 AND COALESCE(count_value, 0) > 0",
                params![app, before_date],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
            )
            .unwrap_or((0, 0));
    }
    // 事件时刻本地化（审查 HIGH：DST 修复，见 queries::LOCAL_MODIFIER_AT_EVENT）
    let off = super::LOCAL_MODIFIER_AT_EVENT;
    // 审查 HIGH：历史基线必须与"今日"侧及 agg 路径同口径——过滤
    // keyboard/mouse/window。不过滤时历史里只有 clipboard/系统事件的 app 会
    // 把日均基线抬高，真实突增被静默漏报，且 hist_days>0 会吞掉"新应用首次
    // 出现"分支，与 agg_daily 缓存路径结论分叉。
    conn.query_row(
        "SELECT COUNT(*), COUNT(DISTINCT substr(datetime(timestamp, ?3), 1, 10)) \
         FROM events \
         WHERE app_name = ?1 AND substr(datetime(timestamp, ?3), 1, 10) < ?2 \
           AND event_type IN ('keyboard','mouse','window')",
        params![app, before_date, off],
        |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
    )
    .unwrap_or((0, 0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;

    fn conn() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(crate::db::SCHEMA).unwrap();
        let _ = crate::db::run_migrations(&c);
        c
    }

    /// 插入一条 keyboard press 事件（timestamp 为 UTC RFC3339）。
    fn press(c: &Connection, ts_utc: &str) {
        c.execute(
            "INSERT INTO events (timestamp, event_type, event_action, session_id) \
             VALUES (?1, 'keyboard', 'press', 1)",
            params![ts_utc],
        )
        .unwrap();
    }

    const OFF8: &str = "+28800 seconds";
    /// UTC+8 下「本地 2026-06-15」对应的 UTC `[start, end)` 边界。
    const RANGE: (&str, &str) = ("2026-06-14T16:00:00+00:00", "2026-06-15T16:00:00+00:00");

    /// UTC+8 下本地 07:30（UTC 23:30 前一日）不是深夜：修复前 UTC 小时=23 会被误报。
    #[test]
    fn local_morning_0730_is_not_late_night_at_utc8() {
        let c = conn();
        press(&c, "2026-06-14T23:30:00+00:00"); // 本地 2026-06-15 07:30
        press(&c, "2026-06-15T00:00:00+00:00"); // 本地 08:00
        let n = late_night_key_count_in_range(&c, RANGE.0, RANGE.1, 23, 6, OFF8);
        assert_eq!(n, 0, "本地早晨 07:30 不应计为深夜");
    }

    /// UTC+8 下本地 23:30（UTC 15:30）是深夜：修复前 UTC 小时=15 会被漏报。
    #[test]
    fn local_night_2330_is_late_night_at_utc8() {
        let c = conn();
        press(&c, "2026-06-15T15:30:00+00:00"); // 本地 23:30
        press(&c, "2026-06-15T05:00:00+00:00"); // 本地 13:00，白天不计
        let n = late_night_key_count_in_range(&c, RANGE.0, RANGE.1, 23, 6, OFF8);
        assert_eq!(n, 1, "本地深夜 23:30 应计入");
    }

    /// 统一口径（2026-09）：窗口 [23, 06) 跨午夜——02:00 也计入深夜，
    /// 正午 12:00 不计入。
    #[test]
    fn late_night_window_spans_midnight_23_to_06() {
        let c = conn();
        press(&c, "2026-06-15T15:30:00+00:00"); // 本地 23:30 → 计
        press(&c, "2026-06-14T18:00:00+00:00"); // 本地 02:00（同 UTC 日窗内）→ 计
        press(&c, "2026-06-15T04:00:00+00:00"); // 本地 12:00 → 不计
        press(&c, "2026-06-15T10:00:00+00:00"); // 本地 18:00 → 不计
        let n = late_night_key_count_in_range(&c, RANGE.0, RANGE.1, 23, 6, OFF8);
        assert_eq!(n, 2, "23:30 与 02:00 计入深夜，12:00/18:00 不计入");
    }

    /// 人侧口径（审查修复 2026-09）：纯注入按键不算"深夜按键"——
    /// 注入 500 键 + 真人 30 键的深夜分钟只计 30。
    #[test]
    fn late_night_excludes_injected_keys() {
        let c = conn();
        // 本地 23:30：注入 500（injected=1 的 raw 形态不可得，用 input_agg 形态）
        ins_ev(
            &c,
            "2026-06-15T15:30:00+00:00",
            "keyboard",
            "input_agg",
            Some(r#"{"keys":530,"injected_keys":500,"samples":530}"#),
        );
        let n = late_night_key_count_in_range(&c, RANGE.0, RANGE.1, 23, 6, OFF8);
        assert_eq!(n, 30, "深夜按键必须扣减注入（530-500=30）");
    }

    /// 人侧口径（审查修复 2026-09）：纯注入分钟不进 top_burst_minutes。
    #[test]
    fn top_burst_excludes_injected_only_minutes() {
        let c = conn();
        // 纯注入分钟：keys=500, injected_keys=500 → 人侧 0，不得入榜
        ins_ev(
            &c,
            "2026-06-15T04:00:00+00:00",
            "keyboard",
            "input_agg",
            Some(r#"{"keys":500,"injected_keys":500,"samples":500}"#),
        );
        // 真人分钟：60 键
        ins_ev(
            &c,
            "2026-06-15T05:00:00+00:00",
            "keyboard",
            "input_agg",
            Some(r#"{"keys":60,"samples":60}"#),
        );
        let out = top_burst_minutes_in_range(&c, RANGE.0, RANGE.1, 50, OFF8);
        assert_eq!(
            out,
            vec![("2026-06-15T13:00".to_string(), 60)],
            "纯注入分钟不得计为行为突增，只应返回真人分钟"
        );
    }

    // ─── 交叉审查 P1：四处 active 分钟口径一致性 ─────────────────────────────

    use crate::db::agg as agg_mod;

    /// 注入本地今日 UTC 边界内第 `mins` 分钟的 UTC RFC3339 时间戳。
    fn ts_in_today(mins: i64) -> String {
        let (start, _) = crate::queries::local_day_range(&crate::queries::today_local_str())
            .expect("today 解析必然成功");
        let base = chrono::DateTime::parse_from_rfc3339(&start).unwrap();
        (base + chrono::Duration::minutes(mins)).to_rfc3339()
    }

    fn ins_ev(c: &Connection, ts: &str, t: &str, a: &str, data: Option<&str>) {
        c.execute(
            "INSERT INTO events (timestamp, event_type, event_action, event_data, session_id) \
             VALUES (?1, ?2, ?3, ?4, 1)",
            params![ts, t, a, data],
        )
        .unwrap();
    }

    /// 同一份数据下，四个 active 分钟来源必须相同（剔除 move-only 分钟）：
    /// active_minutes_by_date / day_totals / daily_agg.recompute_day /
    /// active_minutes_today。覆盖 events 回退路径与 agg 缓存路径两条。
    #[test]
    fn active_minutes_consistent_across_four_sources() {
        let c = conn();
        let today = crate::queries::today_local_str();

        // 分钟 +10: press（活跃）；+20: click（活跃）；+30: keyboard input_agg
        // keys=4（活跃）；+40: mouse input_agg 纯 moves（不活跃）；+50: raw
        // mouse move（不活跃）。活跃分钟黄金值 = 3。
        ins_ev(&c, &ts_in_today(10), "keyboard", "press", None);
        ins_ev(&c, &ts_in_today(20), "mouse", "click", None);
        ins_ev(
            &c,
            &ts_in_today(30),
            "keyboard",
            "input_agg",
            Some(r#"{"keys":4,"samples":4}"#),
        );
        ins_ev(
            &c,
            &ts_in_today(40),
            "mouse",
            "input_agg",
            Some(r#"{"moves":9,"move_distance_px":100,"samples":9}"#),
        );
        ins_ev(
            &c,
            &ts_in_today(50),
            "mouse",
            "move",
            Some(r#"{"x":1,"y":2}"#),
        );

        let check_all = |label: &str| {
            let by_date = super::active_minutes_by_date(&c, &today).len() as i64;
            let totals = super::day_totals(&c, &today).unwrap().active_minutes;
            crate::daily_agg::recompute_day(&c, &today).unwrap();
            let daily: i64 = c
                .query_row(
                    "SELECT active_minutes FROM daily_agg WHERE date = ?1",
                    params![today],
                    |r| r.get(0),
                )
                .unwrap();
            let (t_start, t_end) = crate::queries::today_range();
            let today_min = crate::queries::active_minutes_today(&c, &t_start, &t_end);
            assert_eq!(
                (by_date, totals, daily, today_min),
                (3, 3, 3, 3),
                "{label}: 四处 active 分钟必须一致且剔除 move-only（黄金值 3）"
            );
        };

        check_all("events 回退路径");

        // 建 agg 缓存后（agg 读路径）同解
        agg_mod::rebuild_all(&c).unwrap();
        check_all("agg 缓存路径");
    }

    // ─── 交叉审查 P2：突增分钟显示本地时间 ─────────────────────────────────────

    /// UTC+8 下 UTC 04:00（本地 12:00）的突增分钟必须显示 "12:00"，
    /// 修复前回退路径返回 UTC 原串 "2026-06-15T04:00"。
    #[test]
    fn top_burst_minutes_fallback_formats_local_time() {
        let c = conn();
        press(&c, "2026-06-15T04:00:00+00:00"); // 本地（UTC+8）2026-06-15 12:00
        press(&c, "2026-06-15T04:00:30+00:00");
        let out = top_burst_minutes_in_range(&c, RANGE.0, RANGE.1, 1, OFF8);
        assert_eq!(
            out,
            vec![("2026-06-15T12:00".to_string(), 2)],
            "回退路径必须输出本地时区分钟串"
        );
    }
}
