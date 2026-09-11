//! DB 维护统计与清理（ctl db stats / cleanup / export 用）。

use rusqlite::{params, Connection};

use super::{count_or_log, get_count_i64};
use crate::Result;

/// sessions 表总行数
pub fn count_all_sessions(conn: &Connection) -> i64 {
    count_or_log(
        conn.query_row("SELECT COUNT(*) FROM sessions", [], get_count_i64),
        "count_all_sessions",
    )
}

/// 未关闭（end_time IS NULL）的幽灵 session 数
pub fn count_ghost_sessions(conn: &Connection) -> i64 {
    count_or_log(
        conn.query_row(
            "SELECT COUNT(*) FROM sessions WHERE end_time IS NULL",
            [],
            get_count_i64,
        ),
        "count_ghost_sessions",
    )
}

/// 删除 timestamp < cutoff 的事件，返回删除行数
pub fn delete_events_before(conn: &Connection, cutoff: &str) -> Result<usize> {
    Ok(conn.execute("DELETE FROM events WHERE timestamp < ?1", params![cutoff])?)
}

/// 删除已结束且 end_time < cutoff 的 session，返回删除行数
pub fn delete_closed_sessions_before(conn: &Connection, cutoff: &str) -> Result<usize> {
    Ok(conn.execute(
        "DELETE FROM sessions WHERE end_time IS NOT NULL AND end_time < ?1",
        params![cutoff],
    )?)
}

/// 导出用的全字段行（CSV/JSON/JSONL 导出）
#[derive(Debug, Clone)]
pub struct ExportRow {
    pub id: i64,
    pub timestamp: String,
    pub event_type: String,
    pub event_action: String,
    pub event_data: Option<String>,
    pub app_name: Option<String>,
    pub window_title: Option<String>,
    pub session_id: Option<i64>,
}

/// 导出自 cutoff 以来全部事件（按 id 升序），用于 ctl export。
/// 流式导出（审查 P2：原实现把全表 collect 进 Vec，千万行级可达数 GB 内存
/// 且 JSON 双倍峰值）。逐行回调，写一行丢一行。
pub fn export_events_since_stream(
    conn: &Connection,
    cutoff: &str,
    mut sink: impl FnMut(ExportRow),
) {
    let Ok(mut stmt) = conn.prepare(
        "SELECT id, timestamp, event_type, event_action, event_data, app_name, window_title, session_id
         FROM events WHERE timestamp >= ?1 ORDER BY id",
    ) else {
        return;
    };
    let Ok(rows) = stmt.query_map(params![cutoff], |r| {
        Ok(ExportRow {
            id: r.get(0)?,
            timestamp: r.get(1)?,
            event_type: r.get(2)?,
            event_action: r.get(3)?,
            event_data: r.get(4)?,
            app_name: r.get(5)?,
            window_title: r.get(6)?,
            session_id: r.get(7)?,
        })
    }) else {
        return;
    };
    for row in rows.flatten() {
        sink(row);
    }
}

pub fn export_events_since(conn: &Connection, cutoff: &str) -> Vec<ExportRow> {
    let mut out = Vec::new();
    export_events_since_stream(conn, cutoff, |row| out.push(row));
    out
}
