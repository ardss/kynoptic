//! 事件历史快照（get_process / get_network / get_insights 用）。

use rusqlite::{params, Connection};

use crate::types::{EventAction, EventType};

/// 取某 (event_type, event_action) 最近 limit 条的 (timestamp, event_data JSON)。
/// 用于 get_process / get_network 的 history、get_insights 的磁盘历史。
///
/// 用 [`EventType`] / [`EventAction`] 枚举绑定参数，避免字符串 WHERE 拼接。
pub fn event_history(
    conn: &Connection,
    etype: EventType,
    action: EventAction,
    limit: i64,
) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let sql = "SELECT timestamp, event_data FROM events WHERE event_type=?1 AND event_action=?2 AND event_data IS NOT NULL ORDER BY timestamp DESC LIMIT ?3";
    let Ok(mut stmt) = conn.prepare(sql) else {
        return out;
    };
    if let Ok(rows) = stmt.query_map(params![etype.as_str(), action.as_str(), limit], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    }) {
        for row in rows.flatten() {
            out.push(row);
        }
    }
    out
}

/// 取某 (event_type, event_action) 最新一行的 timestamp（不含 event_data）。
/// 用于 get_insights 的"最近一次休息结束时间"。
///
/// 用 [`EventType`] / [`EventAction`] 枚举绑定参数，避免字符串 WHERE 拼接。
pub fn latest_event_ts_by_action(
    conn: &Connection,
    etype: EventType,
    action: EventAction,
) -> Option<String> {
    conn.query_row(
        "SELECT timestamp FROM events WHERE event_type=?1 AND event_action=?2 ORDER BY timestamp DESC LIMIT 1",
        params![etype.as_str(), action.as_str()],
        |r| r.get(0),
    )
    .ok()
}

/// 取某 event_type 下任意 action 的最新 timestamp。
/// 用于 get_insights 的"最近 idle_start 或 idle_end"。
///
/// 用 [`EventType`] / [`EventAction`] 枚举绑定参数，避免字符串 WHERE 拼接。
pub fn latest_event_ts_of_actions(
    conn: &Connection,
    etype: EventType,
    actions: &[EventAction],
) -> Option<String> {
    if actions.is_empty() {
        return None;
    }
    use rusqlite::params_from_iter;
    let strs: Vec<&str> = actions.iter().map(|a| a.as_str()).collect();
    let placeholders: Vec<String> = (0..strs.len()).map(|_| "?".to_string()).collect();
    let sql = format!(
        "SELECT timestamp FROM events WHERE event_type=? AND event_action IN ({}) ORDER BY timestamp DESC LIMIT 1",
        placeholders.join(",")
    );
    let mut params_vec: Vec<String> = Vec::with_capacity(strs.len() + 1);
    params_vec.push(etype.as_str().to_string());
    params_vec.extend(strs.iter().map(|s| s.to_string()));
    conn.query_row(&sql, params_from_iter(params_vec), |r| r.get(0))
        .ok()
}

/// 取某 event_type 下任意 action 的最近 limit 条 event_data JSON。
/// 用于 get_insights 的"最近 USB 插拔事件"。
///
/// 用 [`EventType`] / [`EventAction`] 枚举绑定参数，避免字符串 WHERE 拼接。
pub fn recent_event_data(
    conn: &Connection,
    etype: EventType,
    actions: &[EventAction],
    limit: i64,
) -> Vec<String> {
    let mut out = Vec::new();
    if actions.is_empty() {
        return out;
    }
    use rusqlite::params_from_iter;
    let strs: Vec<&str> = actions.iter().map(|a| a.as_str()).collect();
    let placeholders: Vec<String> = (0..strs.len()).map(|_| "?".to_string()).collect();
    let sql = format!(
        "SELECT event_data FROM events WHERE event_type=? AND event_action IN ({}) AND event_data IS NOT NULL ORDER BY timestamp DESC LIMIT ?",
        placeholders.join(",")
    );
    let mut params_vec: Vec<String> = Vec::with_capacity(strs.len() + 2);
    params_vec.push(etype.as_str().to_string());
    params_vec.extend(strs.iter().map(|s| s.to_string()));
    params_vec.push(limit.to_string());
    let Ok(mut stmt) = conn.prepare(&sql) else {
        return out;
    };
    if let Ok(rows) = stmt.query_map(params_from_iter(params_vec), |r| r.get::<_, String>(0)) {
        for row in rows.flatten() {
            out.push(row);
        }
    }
    out
}

/// 按 event_data JSON 子字段过滤,取 event_type+event_action 下 JSON 内某 key
/// 等于给定值之一的最近 limit 条 event_data。
///
/// 用于 USB monitor 这种把所有变种事件都写为同一个 event_action='usb_device'、
/// 真正的语义在 event_data.action 子字段里的场景。
pub fn recent_event_data_by_json_field(
    conn: &Connection,
    event_type: EventType,
    event_action: EventAction,
    json_key: &str,
    json_values: &[&str],
    limit: i64,
) -> Vec<String> {
    let mut out = Vec::new();
    if json_values.is_empty() {
        return out;
    }
    use rusqlite::params_from_iter;
    let placeholders: Vec<String> = (0..json_values.len()).map(|_| "?".to_string()).collect();
    let json_path = format!("$.{}", json_key);
    let sql = format!(
        "SELECT event_data FROM events \
         WHERE event_type=? AND event_action=? \
           AND event_data IS NOT NULL \
           AND json_extract(event_data, ?) IN ({}) \
         ORDER BY timestamp DESC LIMIT ?",
        placeholders.join(",")
    );
    let mut params_vec: Vec<String> = Vec::with_capacity(json_values.len() + 4);
    params_vec.push(event_type.as_str().to_string());
    params_vec.push(event_action.as_str().to_string());
    params_vec.push(json_path);
    params_vec.extend(json_values.iter().map(|s| s.to_string()));
    params_vec.push(limit.to_string());
    let Ok(mut stmt) = conn.prepare(&sql) else {
        return out;
    };
    if let Ok(rows) = stmt.query_map(params_from_iter(params_vec), |r| r.get::<_, String>(0)) {
        for row in rows.flatten() {
            out.push(row);
        }
    }
    out
}

/// 取某 (event_type, event_action) 最新一行的 event_data（解析为 JSON）。
///
/// 用 [`EventType`] / [`EventAction`] 枚举绑定参数，替代旧的
/// `event_type='x' AND event_action='y'` 字符串 WHERE 拼接——枚举改名时编译器
/// 强制跟随，避免静默失效。
pub fn latest_event_data(
    conn: &Connection,
    etype: EventType,
    action: EventAction,
) -> Option<serde_json::Value> {
    conn.query_row(
        "SELECT event_data FROM events \
         WHERE event_type = ?1 AND event_action = ?2 AND event_data IS NOT NULL \
         ORDER BY timestamp DESC LIMIT 1",
        params![etype.as_str(), action.as_str()],
        |r| r.get::<_, String>(0),
    )
    .ok()
    .and_then(|s| serde_json::from_str(&s).ok())
}

/// 单次窗口函数扫描得到多个 (event_type, event_action) 组合各自的最新非 NULL event_data。
///
/// 用于 SystemSnapshot::collect（每 3s 调用），此前对 latest_event_data 调用约 10 次，
/// 每次独立扫描某 event_type 全部行（idx_events_type_ts 不覆盖 event_action 维度）。
/// 此函数用 ROW_NUMBER() 窗口函数单次扫描得到全部组合，将 10 次扫描合并为 1 次。
///
/// 语义与 latest_event_data 完全一致：取每个 (type,action) 组合中
/// event_data IS NOT NULL 且 timestamp 最新的行。
pub fn latest_event_data_multi(
    conn: &Connection,
    targets: &[(&str, &str)],
) -> std::collections::HashMap<(String, String), serde_json::Value> {
    use rusqlite::params_from_iter;
    let mut map: std::collections::HashMap<(String, String), serde_json::Value> =
        std::collections::HashMap::new();
    if targets.is_empty() {
        return map;
    }
    // 构造 (event_type, event_action) IN ((?,?),(?,?),...) 行构造器
    let placeholders: Vec<String> = (0..targets.len()).map(|_| "(?,?)".to_string()).collect();
    let sql = format!(
        "SELECT event_type, event_action, event_data FROM ( \
            SELECT event_type, event_action, event_data, \
                   ROW_NUMBER() OVER (PARTITION BY event_type, event_action ORDER BY timestamp DESC) AS rn \
            FROM events \
            WHERE event_data IS NOT NULL \
              AND (event_type, event_action) IN ({}) \
         ) WHERE rn = 1",
        placeholders.join(",")
    );
    // 参数按 (type, action) 对展开
    let params_iter = targets
        .iter()
        .flat_map(|(t, a)| [t.to_string(), a.to_string()]);
    let Ok(mut stmt) = conn.prepare(&sql) else {
        return map;
    };
    let Ok(rows) = stmt.query_map(params_from_iter(params_iter), |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
        ))
    }) else {
        return map;
    };
    for row in rows.flatten() {
        let (etype, eaction, data_str) = row;
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(&data_str) {
            map.insert((etype, eaction), val);
        }
    }
    map
}
