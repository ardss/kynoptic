//! Session 事件（锁屏/解锁/显示器变化）的查询通道。
//!
//! 此前 Lock/Unlock/DisplayChange 落库后没有任何消费方：查询层 0 处按
//! Session 过滤，托盘与面板都看不到"锁了多久、何时解锁"。本模块给消费方
//! （托盘第四态图标、dash 展示）提供唯一的取数口径：
//! - [`current_lock_state`]：当前是否锁定 + 该状态的起始时刻（托盘 tooltip
//!   「已锁定 X 分钟」直接用 now - since 计算）；
//! - [`locked_minutes_between`]：时间窗内的累计锁定分钟数（dash 透出用），
//!   未闭合的尾部锁定计到窗口终点（调用方要"到此刻"就把 end 传 now）。
//!
//! 禁止消费方再自己写 `WHERE event_action = 'lock'` 类 SQL——口径改动只改这里。

use rusqlite::{params, Connection};

/// 当前锁定状态（由库里最近一条 Session 事件决定）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockState {
    /// true = 最近一条 Session 事件是 lock（当前处于锁定）。
    pub locked: bool,
    /// 决定当前状态的那条事件的 UTC RFC3339 时间戳
    /// （锁定中 = 锁定开始时刻；未锁定 = 最近一次解锁时刻）。
    pub since: Option<String>,
}

impl LockState {
    /// 从未锁定、无记录的空状态。
    pub fn unlocked() -> Self {
        Self {
            locked: false,
            since: None,
        }
    }
}

/// 读库里最近一条 lock/unlock 事件，推导当前锁定状态。
///
/// 查询失败（表缺列等）回退"未锁定"并 log warn，与 queries 层容错口径一致。
pub fn current_lock_state(conn: &Connection) -> LockState {
    let res = conn.query_row(
        "SELECT event_action, timestamp FROM events \
         WHERE event_type = 'session' AND event_action IN ('lock', 'unlock') \
         ORDER BY rowid DESC LIMIT 1",
        [],
        |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
    );
    match res {
        Ok((action, ts)) => {
            let locked = action == "lock";
            LockState {
                locked,
                since: Some(ts),
            }
        }
        Err(e) => {
            if !matches!(e, rusqlite::Error::QueryReturnedNoRows) {
                log::warn!("查询失败(current_lock_state): {e}");
            }
            LockState::unlocked()
        }
    }
}

/// 时间窗 `[start, end)`（UTC RFC3339）内的累计锁定时长（分钟，向下取整）。
///
/// 折算口径：lock/unlock 成对扣减；窗口起点前已锁定（尾部未闭合的 lock）
/// 从窗口起点起算；窗口结束时仍未解锁的尾部锁定计到窗口终点。返回 0 在
/// "无记录"与"查询失败"时都会出现——查询失败会 log warn 留痕。
pub fn locked_minutes_between(conn: &Connection, start: &str, end: &str) -> i64 {
    let stmt = conn.prepare(
        "SELECT event_action, timestamp FROM events \
         WHERE event_type = 'session' AND event_action IN ('lock', 'unlock') \
           AND timestamp < ?2 AND (timestamp >= ?1 OR event_action = 'lock') \
         ORDER BY timestamp ASC, rowid ASC",
    );
    let mut stmt = match stmt {
        Ok(s) => s,
        Err(e) => {
            log::warn!("查询失败(locked_minutes_between): {e}");
            return 0;
        }
    };
    let pairs = stmt.query_map(params![start, end], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    });
    let rows: Vec<(String, String)> = match pairs {
        Ok(it) => super::collect_rows_warn(it, "locked_minutes_between"),
        Err(e) => {
            log::warn!("查询失败(locked_minutes_between): {e}");
            return 0;
        }
    };

    // 线性扫描成对折算（事件量级极小：每天个位数）。
    // 状态用一个"锁定起始哨兵"表达：None=未锁定，Some(t)=自 t 起锁定。
    let clip_secs = |t: &str| -> Option<f64> {
        // RFC3339 → epoch 秒；解析失败按该事件缺数据跳过（留痕走跳过行日志）
        chrono::DateTime::parse_from_rfc3339(t)
            .ok()
            .map(|d| d.timestamp() as f64)
    };
    let start_secs = match clip_secs(start) {
        Some(s) => s,
        None => return 0,
    };
    let end_secs = match clip_secs(end) {
        Some(s) => s,
        None => return 0,
    };
    let mut locked_since: Option<f64> = None;
    let mut total = 0.0f64;
    for (action, ts) in &rows {
        let t = match clip_secs(ts) {
            Some(t) => t,
            None => continue,
        };
        if action == "lock" {
            if locked_since.is_none() {
                locked_since = Some(t.max(start_secs));
            }
        } else if let Some(s) = locked_since.take() {
            total += (t.min(end_secs) - s).max(0.0);
        }
    }
    // 尾部未闭合：计到窗口终点
    if let Some(s) = locked_since {
        total += (end_secs - s).max(0.0);
    }
    (total / 60.0).floor() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE events (\
                 id INTEGER PRIMARY KEY AUTOINCREMENT, \
                 timestamp TEXT NOT NULL, \
                 event_type TEXT NOT NULL, \
                 event_action TEXT NOT NULL, \
                 event_data TEXT);",
        )
        .unwrap();
        conn
    }

    fn insert(conn: &Connection, ts: &str, action: &str) {
        conn.execute(
            "INSERT INTO events (timestamp, event_type, event_action) VALUES (?1, 'session', ?2)",
            params![ts, action],
        )
        .unwrap();
    }

    #[test]
    fn no_events_reports_unlocked() {
        let conn = mem_db();
        assert_eq!(current_lock_state(&conn), LockState::unlocked());
        assert_eq!(
            locked_minutes_between(&conn, "2026-09-25T00:00:00Z", "2026-09-25T01:00:00Z"),
            0
        );
    }

    #[test]
    fn latest_event_decides_state() {
        let conn = mem_db();
        insert(&conn, "2026-09-25T08:00:00Z", "lock");
        insert(&conn, "2026-09-25T09:00:00Z", "unlock");
        let st = current_lock_state(&conn);
        assert!(!st.locked);
        assert_eq!(st.since.as_deref(), Some("2026-09-25T09:00:00Z"));
        // 再锁一次：状态翻转，since 指向新 lock
        insert(&conn, "2026-09-25T10:00:00Z", "lock");
        let st = current_lock_state(&conn);
        assert!(st.locked);
        assert_eq!(st.since.as_deref(), Some("2026-09-25T10:00:00Z"));
    }

    #[test]
    fn minutes_pair_and_window_clipping() {
        let conn = mem_db();
        // 窗口前已锁定（跨起点）、窗口内解锁；之后又锁、窗口内未解锁
        insert(&conn, "2026-09-24T23:00:00Z", "lock");
        insert(&conn, "2026-09-25T00:20:00Z", "unlock"); // 起点 00:00 → 20 分钟
        insert(&conn, "2026-09-25T00:30:00Z", "lock");
        insert(&conn, "2026-09-25T01:15:00Z", "unlock"); // 45 分钟
        insert(&conn, "2026-09-25T01:50:00Z", "lock"); // 尾部计到 01:00？不——窗口 end=02:00 → 10 分钟
        let got = locked_minutes_between(&conn, "2026-09-25T00:00:00Z", "2026-09-25T02:00:00Z");
        assert_eq!(got, 20 + 45 + 10);
        // 窗口收窄到 01:00 前：尾部未闭合的 lock 计到窗口终点
        let got2 = locked_minutes_between(&conn, "2026-09-25T00:00:00Z", "2026-09-25T01:00:00Z");
        assert_eq!(got2, 20 + 30);
    }

    #[test]
    fn non_session_events_ignored() {
        let conn = mem_db();
        insert(&conn, "2026-09-25T08:00:00Z", "lock");
        conn.execute(
            "INSERT INTO events (timestamp, event_type, event_action) VALUES ('2026-09-25T09:00:00Z', 'keyboard', 'press')",
            [],
        )
        .unwrap();
        assert!(current_lock_state(&conn).locked);
    }
}
