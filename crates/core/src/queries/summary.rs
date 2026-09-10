//! 今日 / 全量聚合计数（keys / clicks / active_minutes / sessions 等）。
//!
//! 供 dashboard summary 面板、tray 实时数据、snapshot 缓存等消费。

use rusqlite::{params, Connection};

use super::{count_or_log, get_count_i64, CLICKS_ROW_EXPR, KEYS_ROW_EXPR};

pub fn count_today_keys(conn: &Connection, today: &str, tomorrow: &str) -> i64 {
    count_keys_in_range(conn, today, tomorrow)
}

pub fn count_today_clicks(conn: &Connection, today: &str, tomorrow: &str) -> i64 {
    count_clicks_in_range(conn, today, tomorrow)
}

/// 任意 [start, end) 区间内的按键（keyboard/press）数。
/// count_today_keys 的通用版本，供 tray.rs 等任意区间查询复用。
/// 兼容 input_agg 计数行（minute 粒度，见 mod.rs 的 KEYS_ROW_EXPR）。
pub fn count_keys_in_range(conn: &Connection, start: &str, end: &str) -> i64 {
    count_or_log(
        conn.query_row(
            &format!(
                "SELECT COALESCE(SUM({KEYS_ROW_EXPR}), 0) FROM events \
                 WHERE event_type='keyboard' AND event_action IN ('press','input_agg') \
                   AND timestamp >= ?1 AND timestamp < ?2"
            ),
            params![start, end],
            get_count_i64,
        ),
        "count_keys_in_range",
    )
}

/// 任意 [start, end) 区间内的点击（mouse/click）数。
pub fn count_clicks_in_range(conn: &Connection, start: &str, end: &str) -> i64 {
    count_or_log(
        conn.query_row(
            &format!(
                "SELECT COALESCE(SUM({CLICKS_ROW_EXPR}), 0) FROM events \
                 WHERE event_type='mouse' AND event_action IN ('click','input_agg') \
                   AND timestamp >= ?1 AND timestamp < ?2"
            ),
            params![start, end],
            get_count_i64,
        ),
        "count_clicks_in_range",
    )
}

pub fn count_today_action(
    conn: &Connection,
    event_type: &str,
    event_action: &str,
    today: &str,
    tomorrow: &str,
) -> i64 {
    count_or_log(
        conn.query_row(
            "SELECT COUNT(*) FROM events WHERE event_type=?1 AND event_action=?2 AND timestamp >= ?3 AND timestamp < ?4",
            params![event_type, event_action, today, tomorrow],
            get_count_i64,
        ),
        "count_today_action",
    )
}

pub fn count_today_events(conn: &Connection, today: &str, tomorrow: &str) -> i64 {
    count_or_log(
        conn.query_row(
            "SELECT COUNT(*) FROM events WHERE timestamp >= ?1 AND timestamp < ?2",
            params![today, tomorrow],
            get_count_i64,
        ),
        "count_today_events",
    )
}

/// 单次 GROUP BY 扫描得到今日所有 (event_type, event_action) 计数 + 总数。
///
/// 用于替代 get_realtime 中多次独立的 COUNT 查询（热路径优化）：
/// 将 N 次表扫描合并为 1 次。返回 (总事件数, 组合计数表)。
/// 查询失败时返回 (0, 空 map)，等价于所有计数为 0，与逐条查询的容错一致。
pub fn today_action_breakdown(
    conn: &Connection,
    today: &str,
    tomorrow: &str,
) -> (i64, std::collections::HashMap<(String, String), i64>) {
    let mut map: std::collections::HashMap<(String, String), i64> =
        std::collections::HashMap::new();
    let mut total: i64 = 0;
    let Ok(mut stmt) = conn.prepare(
        "SELECT event_type, event_action, COUNT(*) FROM events WHERE timestamp >= ?1 AND timestamp < ?2 GROUP BY event_type, event_action"
    ) else {
        return (0, map);
    };
    let Ok(rows) = stmt.query_map(params![today, tomorrow], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, i64>(2)?,
        ))
    }) else {
        return (0, map);
    };
    for row in rows.flatten() {
        let (etype, eaction, cnt) = row;
        total += cnt;
        map.insert((etype, eaction), cnt);
    }
    (total, map)
}

pub fn active_minutes_today(conn: &Connection, today: &str, tomorrow: &str) -> i64 {
    // 去重桶按本地分钟取，避免本地日跨 UTC 午夜时两端同名 HH:MM 被错误合并。
    let off = super::local_offset_modifier();
    count_or_log(
        conn.query_row(
            "SELECT COUNT(DISTINCT substr(datetime(timestamp, ?1), 12, 5)) FROM events WHERE timestamp >= ?2 AND timestamp < ?3",
            params![&off, today, tomorrow],
            get_count_i64,
        ),
        "active_minutes_today",
    )
}

// === 全量统计 ===

/// 有数据的天数（DISTINCT **本地**日期）。用于 summary 面板。
pub fn distinct_active_days(conn: &Connection) -> i64 {
    let off = super::local_offset_modifier();
    count_or_log(
        conn.query_row(
            "SELECT COUNT(DISTINCT substr(datetime(timestamp, ?1), 1, 10)) FROM events",
            params![&off],
            get_count_i64,
        ),
        "distinct_active_days",
    )
}

/// 全量事件总数。用于 summary 面板与 ctl db stats。
pub fn count_all_events(conn: &Connection) -> i64 {
    count_or_log(
        conn.query_row("SELECT COUNT(*) FROM events", [], get_count_i64),
        "count_all_events",
    )
}

pub fn count_all_keys(conn: &Connection) -> i64 {
    count_or_log(
        conn.query_row(
            &format!(
                "SELECT COALESCE(SUM({KEYS_ROW_EXPR}), 0) FROM events \
                 WHERE event_type='keyboard' AND event_action IN ('press','input_agg')"
            ),
            [],
            get_count_i64,
        ),
        "count_all_keys",
    )
}

pub fn count_all_clicks(conn: &Connection) -> i64 {
    count_or_log(
        conn.query_row(
            &format!(
                "SELECT COALESCE(SUM({CLICKS_ROW_EXPR}), 0) FROM events \
                 WHERE event_type='mouse' AND event_action IN ('click','input_agg')"
            ),
            [],
            get_count_i64,
        ),
        "count_all_clicks",
    )
}

pub fn count_all_active_minutes(conn: &Connection) -> i64 {
    let off = super::local_offset_modifier();
    count_or_log(
        conn.query_row(
            "SELECT COUNT(DISTINCT substr(datetime(timestamp, ?1), 1, 16)) FROM events WHERE event_type IN ('keyboard','mouse') AND event_action IN ('press','click')",
            params![&off],
            get_count_i64,
        ),
        "count_all_active_minutes",
    )
}

/// 昨天「同一时刻为止」的输入累计——供今日 vs 昨日同期对比。
///
/// 时区语义同 [`super::today_range`]：起点取**本地昨日午夜**（转 UTC），
/// 终点取当前时刻往前推一天（UTC RFC3339）。避免 UTC 切日导致对比错位。
pub fn yesterday_total_same_time(conn: &Connection) -> i64 {
    // 起点复用 local_day_range(昨日) 取本地昨日午夜的 UTC 边界，与时区工具同源。
    let yesterday = super::date_offset_str(-1);
    let yesterday_start = super::local_day_range(&yesterday)
        .map(|(start, _)| start)
        .unwrap_or_else(|| format!("{yesterday}T00:00:00+00:00"));
    // 终点 = 当前时刻往前推一天（时间点运算，UTC 正确，与 timestamp 同时区比较）。
    let yesterday_same_time = (chrono::Utc::now() - chrono::Duration::days(1)).to_rfc3339();
    count_or_log(
        conn.query_row(
            &format!(
                "SELECT COALESCE(SUM({KEYS_ROW_EXPR} + {CLICKS_ROW_EXPR}), 0) FROM events \
                 WHERE event_type IN ('keyboard','mouse') \
                   AND event_action IN ('press','click','input_agg') \
                   AND timestamp >= ?1 AND timestamp < ?2"
            ),
            params![&yesterday_start, yesterday_same_time],
            get_count_i64,
        ),
        "yesterday_total_same_time",
    )
}

pub fn count_recent_input(conn: &Connection, since: &str) -> i64 {
    count_or_log(
        conn.query_row(
            &format!(
                "SELECT COALESCE(SUM({KEYS_ROW_EXPR} + {CLICKS_ROW_EXPR}), 0) FROM events \
                 WHERE event_type IN ('keyboard','mouse') \
                   AND event_action IN ('press','click','input_agg') AND timestamp >= ?1"
            ),
            params![since],
            get_count_i64,
        ),
        "count_recent_input",
    )
}

pub fn count_keys_since(conn: &Connection, since_id: i64) -> i64 {
    count_or_log(
        conn.query_row(
            &format!(
                "SELECT COALESCE(SUM({KEYS_ROW_EXPR}), 0) FROM events \
                 WHERE event_type='keyboard' AND event_action IN ('press','input_agg') AND id > ?1"
            ),
            params![since_id],
            get_count_i64,
        ),
        "count_keys_since",
    )
}

pub fn count_clicks_since(conn: &Connection, since_id: i64) -> i64 {
    count_or_log(
        conn.query_row(
            &format!(
                "SELECT COALESCE(SUM({CLICKS_ROW_EXPR}), 0) FROM events \
                 WHERE event_type='mouse' AND event_action IN ('click','input_agg') AND id > ?1"
            ),
            params![since_id],
            get_count_i64,
        ),
        "count_clicks_since",
    )
}

/// 单次条件聚合扫描得到自 since_id 以来的按键数 + 点击数。
///
/// 用于 PetStateSync 线程（每 30s），将 keys/clicks 两次表扫描合并为一次。
/// 查询失败时返回 (0, 0)，与逐条查询的容错一致。
pub fn keys_clicks_since(conn: &Connection, since_id: i64) -> (i64, i64) {
    let res = conn.query_row(
        &format!(
            "SELECT \
                COALESCE(SUM({KEYS_ROW_EXPR}), 0), \
                COALESCE(SUM({CLICKS_ROW_EXPR}), 0) \
             FROM events WHERE event_type IN ('keyboard','mouse') \
               AND event_action IN ('press','click','input_agg') AND id > ?1"
        ),
        params![since_id],
        |r| {
            // SUM 在无匹配行时返回 NULL，需转 0
            let keys = r.get::<_, i64>(0)?;
            let clicks = r.get::<_, i64>(1)?;
            Ok((keys, clicks))
        },
    );
    res.unwrap_or_else(|e| {
        if !matches!(e, rusqlite::Error::QueryReturnedNoRows) {
            log::warn!("查询失败(keys_clicks_since): {e}");
        }
        (0, 0)
    })
}

/// 单次条件聚合扫描得到 [today, tomorrow) 区间的按键数 + 点击数。
///
/// 用于 SystemSnapshot::collect，将 count_today_keys + count_today_clicks
/// 两次表扫描合并为一次。
pub fn keys_clicks_today(conn: &Connection, today: &str, tomorrow: &str) -> (i64, i64) {
    // 日期必须经 local_day_range 换算成 UTC RFC3339 边界：events.timestamp
    // 是 UTC 字符串，直接与本地日期字符串比较会在 UTC+X 凌晨整段错位
    // （实测：本地 0-8 点"今日键鼠"恒为 0）。tomorrow 参数保留兼容旧签名。
    let _ = tomorrow;
    let Some((start, end)) = crate::queries::local_day_range(today) else {
        return (0, 0);
    };
    let res = conn.query_row(
        &format!(
            "SELECT \
                COALESCE(SUM({KEYS_ROW_EXPR}), 0), \
                COALESCE(SUM({CLICKS_ROW_EXPR}), 0) \
             FROM events WHERE event_type IN ('keyboard','mouse') \
               AND event_action IN ('press','click','input_agg') \
               AND timestamp >= ?1 AND timestamp < ?2"
        ),
        params![start, end],
        |r| {
            let keys = r.get::<_, i64>(0)?;
            let clicks = r.get::<_, i64>(1)?;
            Ok((keys, clicks))
        },
    );
    res.unwrap_or_else(|e| {
        if !matches!(e, rusqlite::Error::QueryReturnedNoRows) {
            log::warn!("查询失败(keys_clicks_today): {e}");
        }
        (0, 0)
    })
}

pub fn count_active_min_since(conn: &Connection, since_id: i64) -> i64 {
    count_or_log(
        conn.query_row(
            "SELECT COUNT(DISTINCT substr(timestamp,1,16)) FROM events WHERE event_type IN ('keyboard','mouse') AND event_action IN ('press','click','input_agg') AND id > ?1",
            params![since_id],
            get_count_i64,
        ),
        "count_active_min_since",
    )
}

pub fn max_event_id(conn: &Connection) -> Option<i64> {
    conn.query_row("SELECT MAX(id) FROM events", [], |r| r.get(0))
        .ok()
}
