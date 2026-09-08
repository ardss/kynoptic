//! 图表数据 + 当前值类查询（dashboard 图表 + analyzer/anomaly 读取）。

use rusqlite::{params, Connection};

use super::{get_string, string_or_log, DayTotals, MinuteStat};

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
        for row in rows.flatten() {
            out.push(row);
        }
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
    let Ok(mut stmt) = conn.prepare(
        "SELECT COALESCE(app_name, '(unknown)'), COUNT(*) as cnt FROM events WHERE timestamp >= ?1 AND timestamp < ?2 AND app_name IS NOT NULL GROUP BY app_name ORDER BY cnt DESC LIMIT ?3"
    ) else {
        return out;
    };
    if let Ok(rows) = stmt.query_map(params![today, tomorrow, limit], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
    }) {
        for row in rows.flatten() {
            out.push(row);
        }
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
        for row in rows.flatten() {
            out.push(row);
        }
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
    let off = super::local_offset_modifier();
    let sql =
        "SELECT CAST(substr(datetime(timestamp, ?1), 12, 2) AS INTEGER) as hour, COUNT(*) as cnt \
         FROM events WHERE timestamp >= ?2 AND timestamp < ?3 GROUP BY hour ORDER BY hour";
    let Ok(mut stmt) = conn.prepare(sql) else {
        return out;
    };
    if let Ok(rows) = stmt.query_map(params![&off, today, tomorrow], |r| {
        Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
    }) {
        for row in rows.flatten() {
            out.push(row);
        }
    }
    out
}

/// 今日逐分钟活动计数，按分钟升序。返回 ("HH:MM", count)。
///
/// 桶按**本地分钟**取（同 [`hourly_counts_today`]），否则分钟分布整体错位时区偏移量。
pub fn minute_counts_today(conn: &Connection, today: &str, tomorrow: &str) -> Vec<(String, i64)> {
    let mut out = Vec::new();
    let off = super::local_offset_modifier();
    let sql = "SELECT substr(datetime(timestamp, ?1), 12, 5) as minute, COUNT(*) as cnt \
         FROM events WHERE timestamp >= ?2 AND timestamp < ?3 GROUP BY minute ORDER BY minute";
    let Ok(mut stmt) = conn.prepare(sql) else {
        return out;
    };
    if let Ok(rows) = stmt.query_map(params![&off, today, tomorrow], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
    }) {
        for row in rows.flatten() {
            out.push(row);
        }
    }
    out
}

/// 自 since 起每日事件计数，按**本地日期**分组。返回 ("YYYY-MM-DD", count)。
///
/// 用于 get_trend（近 7 天趋势）。桶按本地日期取，否则趋势图会把本地凌晨数据归到前一日。
/// WHERE 走 timestamp 范围索引，GROUP BY 用 datetime(timestamp, modifier) 投影。
pub fn daily_counts_since(conn: &Connection, since: &str) -> Vec<(String, i64)> {
    let mut out = Vec::new();
    let off = super::local_offset_modifier();
    let sql = "SELECT substr(datetime(timestamp, ?1), 1, 10) as date, COUNT(*) as cnt \
         FROM events WHERE timestamp >= ?2 GROUP BY date";
    let Ok(mut stmt) = conn.prepare(sql) else {
        return out;
    };
    if let Ok(rows) = stmt.query_map(params![&off, since], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
    }) {
        for row in rows.flatten() {
            out.push(row);
        }
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
        for row in rows.flatten() {
            out.push(row);
        }
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
        for row in rows.flatten() {
            out.push(row);
        }
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
        "SELECT MAX(timestamp) FROM events WHERE event_type IN ('keyboard','mouse') AND event_action IN ('press','click','scroll','move')",
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
    // 按用户本地时区切日（与 today_range 同源）。解析失败回退 UTC 前缀匹配。
    let (start, end) = match super::local_day_range(date) {
        Some(r) => r,
        None => (date.to_string(), format!("{date}\u{7f}")),
    };
    let Ok(mut stmt) = conn.prepare(
        "SELECT substr(timestamp, 1, 16) AS minute, \
                SUM(CASE WHEN event_type='keyboard' AND event_action='press' THEN 1 ELSE 0 END) AS keys, \
                SUM(CASE WHEN event_type='mouse' AND event_action='click' THEN 1 ELSE 0 END) AS clicks, \
                SUM(CASE WHEN event_type='window' AND event_action='switch' THEN 1 ELSE 0 END) AS switches \
         FROM events \
         WHERE timestamp >= ?1 AND timestamp < ?2 \
         GROUP BY minute \
         ORDER BY minute",
    ) else {
        return out;
    };
    let Ok(rows) = stmt.query_map(params![start, end], |r| {
        Ok(MinuteStat {
            minute: r.get::<_, String>(0)?,
            keys: r.get::<_, Option<i64>>(1)?.unwrap_or(0),
            clicks: r.get::<_, Option<i64>>(2)?.unwrap_or(0),
            switches: r.get::<_, Option<i64>>(3)?.unwrap_or(0),
        })
    }) else {
        return out;
    };
    for row in rows.flatten() {
        out.push(row);
    }
    out
}

/// 当日 distinct 活跃分钟列表（ISO "YYYY-MM-DDTHH:MM"），按时间升序。
///
/// 供 [`crate::analyzer::fragmentation_score`] 与
/// [`crate::anomaly::detect_marathon_session`] 共享——两者此前各写一份
/// `SELECT DISTINCT substr(timestamp,1,16) WHERE event_type IN ('keyboard','mouse')`。
pub fn active_minutes_by_date(conn: &Connection, date: &str) -> Vec<String> {
    let mut out = Vec::new();
    let (start, end) = match super::local_day_range(date) {
        Some(r) => r,
        None => (date.to_string(), format!("{date}\u{7f}")),
    };
    let Ok(mut stmt) = conn.prepare(
        "SELECT DISTINCT substr(timestamp, 1, 16) AS minute \
         FROM events \
         WHERE timestamp >= ?1 AND timestamp < ?2 \
           AND (event_type='keyboard' OR event_type='mouse') \
         ORDER BY minute",
    ) else {
        return out;
    };
    let Ok(rows) = stmt.query_map(params![start, end], |r| r.get::<_, String>(0)) else {
        return out;
    };
    for row in rows.flatten() {
        out.push(row);
    }
    out
}

/// 当日 keys/clicks/active_minutes 三合一（`analyze_day` 编排层用）。
///
/// 此前 [`crate::analyzer::analyze_day`] 手写一条带子查询的 SELECT；现在下沉到
/// 数据访问层，业务层只做 apm_avg 计算。
pub fn day_totals(conn: &Connection, date: &str) -> DayTotals {
    let (start, end) = match super::local_day_range(date) {
        Some(r) => r,
        None => (date.to_string(), format!("{date}\u{7f}")),
    };
    let Ok((keys, clicks, active_minutes)) = conn.query_row(
        "SELECT \
            COALESCE(SUM(CASE WHEN event_type='keyboard' AND event_action='press' THEN 1 ELSE 0 END), 0), \
            COALESCE(SUM(CASE WHEN event_type='mouse' AND event_action='click' THEN 1 ELSE 0 END), 0), \
            (SELECT COUNT(DISTINCT substr(timestamp,1,16)) FROM events \
             WHERE timestamp >= ?1 AND timestamp < ?2 AND event_type IN ('keyboard','mouse')) \
         FROM events \
         WHERE timestamp >= ?1 AND timestamp < ?2",
        params![start, end],
        |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?)),
    ) else {
        return DayTotals::default();
    };
    DayTotals {
        keys,
        clicks,
        active_minutes,
    }
}

/// 在 `[start, start:59]` 分钟区间内取最频繁的 (app_name, window_title)。
///
/// 供 [`crate::analyzer`] 为专注段补全 app/window 标签。`end` 是 "YYYY-MM-DDTHH:MM"
/// 形式，本函数自动补 `:59` 以覆盖该分钟内的全部秒。
pub fn top_app_window_in_range(
    conn: &Connection,
    start: &str,
    end: &str,
) -> Option<(String, String)> {
    conn.query_row(
        "SELECT app_name, window_title \
         FROM events \
         WHERE timestamp >= ?1 AND timestamp <= ?2 \
           AND event_type IN ('window', 'keyboard', 'mouse') \
           AND app_name IS NOT NULL \
         GROUP BY app_name, window_title \
         ORDER BY COUNT(*) DESC \
         LIMIT 1",
        params![start, &format!("{}:59", end)],
        |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
    )
    .ok()
}

// ─── 异常检测数据读取（供 [`crate::anomaly`] 消费） ──────────────────────────

/// 当日 `hour_threshold` 时之后的键盘按键数。
///
/// 供 [`crate::anomaly`] 的深夜活动检测——此前该业务模块手写
/// `COUNT(*) WHERE CAST(substr(timestamp,12,2)) >= ?`。
pub fn late_night_key_count(conn: &Connection, date: &str, hour_threshold: i64) -> i64 {
    conn.query_row(
        "SELECT COUNT(*) FROM events \
         WHERE substr(timestamp, 1, 10) = ?1 \
           AND CAST(substr(timestamp, 12, 2) AS INTEGER) >= ?2 \
           AND event_type = 'keyboard' AND event_action = 'press'",
        params![date, hour_threshold],
        |r| r.get::<_, i64>(0),
    )
    .unwrap_or(0)
}

/// `date` 之前所有 `daily_agg` 行的 apm_avg 均值（无数据返回 0.0）。
///
/// 供 [`crate::anomaly`] 的 APM 突增检测作为历史基线。
pub fn daily_agg_avg_apm_before(conn: &Connection, date: &str) -> f64 {
    conn.query_row(
        "SELECT COALESCE(AVG(apm_avg), 0.0) FROM daily_agg WHERE date < ?1",
        params![date],
        |r| r.get::<_, f64>(0),
    )
    .unwrap_or(0.0)
}

/// 当日 top-5 高强度分钟（keys+clicks 合计 ≥ `min_count`），按强度倒序。
///
/// 返回 (minute, count)，供 [`crate::anomaly`] 的 APM 突增检测消费。
pub fn top_burst_minutes(conn: &Connection, date: &str, min_count: i64) -> Vec<(String, i64)> {
    let mut out = Vec::new();
    let Ok(mut stmt) = conn.prepare(
        "SELECT substr(timestamp, 1, 16), COUNT(*) AS n \
         FROM events \
         WHERE substr(timestamp, 1, 10) = ?1 \
           AND event_type IN ('keyboard', 'mouse') \
         GROUP BY 1 \
         HAVING n >= ?2 \
         ORDER BY n DESC \
         LIMIT 5",
    ) else {
        return out;
    };
    let Ok(rows) = stmt.query_map(params![date, min_count], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
    }) else {
        return out;
    };
    for row in rows.flatten() {
        out.push(row);
    }
    out
}

/// 当日 top-N 应用（按事件数倒序），过滤 keyboard/mouse/window 事件且 app_name 非空。
///
/// 供 [`crate::anomaly`] 的新应用突增检测消费。
pub fn top_apps_by_event_types(conn: &Connection, date: &str, limit: i64) -> Vec<(String, i64)> {
    let mut out = Vec::new();
    let Ok(mut stmt) = conn.prepare(
        "SELECT IFNULL(app_name, '(unknown)') AS app, COUNT(*) AS n \
         FROM events \
         WHERE substr(timestamp, 1, 10) = ?1 \
           AND app_name IS NOT NULL \
           AND event_type IN ('keyboard', 'mouse', 'window') \
         GROUP BY app \
         ORDER BY n DESC \
         LIMIT ?2",
    ) else {
        return out;
    };
    let Ok(rows) = stmt.query_map(params![date, limit], |r| {
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
pub fn app_history_totals(conn: &Connection, app: &str, before_date: &str) -> (i64, i64) {
    conn.query_row(
        "SELECT COUNT(*), COUNT(DISTINCT substr(timestamp, 1, 10)) \
         FROM events \
         WHERE app_name = ?1 AND substr(timestamp, 1, 10) < ?2",
        params![app, before_date],
        |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
    )
    .unwrap_or((0, 0))
}
