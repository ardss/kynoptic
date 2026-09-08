//! daily_agg 维护：每天结束时把当天的 keys/clicks/active_min/apm 聚合到 daily_agg
//!
//! 用于异常检测的"历史基线"。
//!
//! **时区**：与 [`crate::queries::today_range`] 同源——"某天"按用户**本地时区**切日
//! （`local_day_range`），active_minutes 去重桶也按本地分钟（`local_offset_modifier`）。
//! `daily_agg.date` 存本地日期字符串，与 `daily_agg_avg_apm_before` 的 `WHERE date < ?` 一致。

use rusqlite::{params, Connection};

use crate::queries;
use crate::Result;

/// 计算某天的 daily_agg 行（如果不存在则插入，已存在则更新）。
/// 返回 0/1 表示是否有变化。`date` 为本地日期 `YYYY-MM-DD`。
///
/// 注意：WHERE 用 `timestamp >= ? AND < ?`（本地午夜→UTC 边界，走 idx_events_timestamp）。
pub fn recompute_day(conn: &Connection, date: &str) -> Result<bool> {
    // 本地日 → [UTC start, UTC end) 边界，与全链路查询口径一致。
    let (start, end) = queries::local_day_range(date).unwrap_or_else(|| {
        // 解析失败时回退到宽松前缀匹配（保留容错）
        (date.to_string(), format!("{date}\u{7f}"))
    });
    let off = queries::local_offset_modifier();

    let (keys, clicks, active_min): (i64, i64, i64) = conn.query_row(
        "SELECT
            COALESCE(SUM(CASE WHEN event_type='keyboard' AND event_action='press' THEN 1 ELSE 0 END), 0),
            COALESCE(SUM(CASE WHEN event_type='mouse' AND event_action='click' THEN 1 ELSE 0 END), 0),
            (SELECT COUNT(DISTINCT substr(datetime(timestamp, ?3), 1, 16)) FROM events
             WHERE timestamp >= ?1 AND timestamp < ?2 AND event_type IN ('keyboard','mouse'))
         FROM events
         WHERE timestamp >= ?1 AND timestamp < ?2",
        params![start, end, off],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;

    let apm = if active_min > 0 {
        (keys + clicks) as f64 / active_min as f64
    } else {
        0.0
    };

    let changed = conn.execute(
        "INSERT INTO daily_agg (date, keys, clicks, active_minutes, apm_avg)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(date) DO UPDATE SET
            keys = excluded.keys,
            clicks = excluded.clicks,
            active_minutes = excluded.active_minutes,
            apm_avg = excluded.apm_avg",
        params![date, keys, clicks, active_min, apm],
    )?;
    Ok(changed > 0)
}

/// 重新计算「最近 days 天」（含今天，按**本地**时区）的 daily_agg 行。
///
/// 供采集器维护线程定期调用：今天仍在累积、昨天可能在 UTC 换日后才补齐，
/// 故默认重算最近 2 天即可让异常检测基线保持新鲜，而无需扫全表（`recompute_all`）。
/// 返回有变化的天数。
pub fn recompute_recent_days(conn: &Connection, days: i64) -> Result<usize> {
    let today = chrono::Local::now();
    let mut n = 0;
    for i in 0..days.max(1) {
        let date = (today - chrono::Duration::days(i))
            .format("%Y-%m-%d")
            .to_string();
        if recompute_day(conn, &date)? {
            n += 1;
        }
    }
    Ok(n)
}

/// 重新计算所有有事件的日期（按**本地**日期 DISTINCT）。
pub fn recompute_all(conn: &Connection) -> Result<usize> {
    let off = queries::local_offset_modifier();
    let dates: Vec<String> = conn
        .prepare("SELECT DISTINCT substr(datetime(timestamp, ?1), 1, 10) FROM events ORDER BY 1")?
        .query_map(params![off], |r| r.get::<_, String>(0))?
        .filter_map(|r| r.ok())
        .collect();
    let mut n = 0;
    for d in dates {
        if recompute_day(conn, &d)? {
            n += 1;
        }
    }
    Ok(n)
}

/// 读取最近 limit 天的 daily_agg 行（按日期倒序）。
/// 返回 (date, keys, clicks, active_minutes, apm_avg)。
/// 供 ctl stats --days N 与未来 dashboard 历史趋势复用，消除消费端手写 SQL。
pub fn recent(conn: &Connection, limit: i64) -> Vec<(String, i64, i64, i64, f64)> {
    let mut out = Vec::new();
    let Ok(mut stmt) = conn.prepare(
        "SELECT date, keys, clicks, active_minutes, apm_avg FROM daily_agg ORDER BY date DESC LIMIT ?1",
    ) else {
        return out;
    };
    let Ok(rows) = stmt.query_map(params![limit], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, i64>(2)?,
            r.get::<_, i64>(3)?,
            r.get::<_, f64>(4)?,
        ))
    }) else {
        return out;
    };
    for row in rows.flatten() {
        out.push(row);
    }
    out
}
