//! 聚合读缓存（agg_minute / agg_daily）——派生数据，原始 events 永不改动
//!
//! **定位（重要契约）**：本模块维护的 agg_minute / agg_daily 行是**纯派生的
//! 只读缓存**，与全量 SQL GROUP BY 现算结果等价。原始 events 表只增不改不删
//! （保留策略除外，与本模块无关）；聚合行随时可用 [`rebuild_all`] 从 events
//! 完整重建。get_anomalies 等全历史扫描查询读缓存提速，原始数据保持完全可查。
//!
//! bucket 语义（agg_minute.date/hour/minute 为**本地时区**分钟桶）：
//! - `input_keys`    sum=按键数, count=原始输入样本数
//! - `input_clicks`  sum=点击数
//! - `input_moves`   sum=移动距离(px), count=采样移动次数
//! - `window_switches` sum=count=窗口切换次数
//!
//! agg_daily 只存 `app:<name>` 行（count=该应用当日事件数），供 new_app_surge
//! 的全历史 per-app 统计从 O(全表) 降到 O(聚合行数)。
//!
//! 维护路径：
//! 1. 增量：writer 每次 flush 后调 [`super::Database::update_agg`]（见 collector.rs）；
//! 2. 懒回填：[`Database::open`] 时若 agg 全空而 events 非空 → [`backfill_if_needed`]
//!    （存量库首开自动补齐；选此方案而非 CLI 命令，见 CODE_NOTES.md §8）。

use chrono::Timelike;
use rusqlite::{params, Connection};

use crate::types::{Event, EventAction, EventType};

/// 本地分钟桶聚合行的写入（增量与重建共用）。
const UPSERT_MINUTE: &str = "
INSERT INTO agg_minute (date, hour, minute, bucket_id, sum_value, count_value)
VALUES (?1, ?2, ?3, ?4, ?5, ?6)
ON CONFLICT(date, hour, minute, bucket_id) DO UPDATE SET
    sum_value = COALESCE(sum_value, 0) + excluded.sum_value,
    count_value = COALESCE(count_value, 0) + excluded.count_value
";

const UPSERT_DAILY_APP: &str = "
INSERT INTO agg_daily (date, bucket_id, sum_value, count_value)
VALUES (?1, 'app:' || ?2, NULL, ?3)
ON CONFLICT(date, bucket_id) DO UPDATE SET
    count_value = COALESCE(count_value, 0) + excluded.count_value
";

/// 单个事件对聚合缓存的增量贡献（本地时区分钟桶）。
fn apply_event(conn: &Connection, e: &Event) -> rusqlite::Result<()> {
    let ts = match chrono::DateTime::parse_from_rfc3339(&e.timestamp) {
        Ok(t) => t,
        Err(_) => return Ok(()), // 时间戳不可解析：跳过（不阻塞整批）
    };
    let local = ts.with_timezone(&chrono::Local);
    let date = local.format("%Y-%m-%d").to_string();
    let hour = local.hour() as i64;
    let minute = local.minute() as i64;

    match (e.event_type, e.event_action) {
        (EventType::Keyboard, EventAction::InputAgg) => {
            let keys = json_counter(e, "keys");
            let samples = json_counter(e, "samples");
            if keys > 0 {
                conn.execute(
                    UPSERT_MINUTE,
                    params![date, hour, minute, "input_keys", keys, samples],
                )?;
            }
        }
        (EventType::Mouse, EventAction::InputAgg) => {
            let clicks = json_counter(e, "clicks");
            let moves = json_counter(e, "moves");
            let dist = json_counter(e, "move_distance_px");
            let samples = json_counter(e, "samples");
            if clicks > 0 {
                conn.execute(
                    UPSERT_MINUTE,
                    params![date, hour, minute, "input_clicks", clicks, samples],
                )?;
            }
            if moves > 0 || dist > 0 {
                conn.execute(
                    UPSERT_MINUTE,
                    params![date, hour, minute, "input_moves", dist, moves],
                )?;
            }
        }
        (EventType::Keyboard, EventAction::Press) => {
            conn.execute(
                UPSERT_MINUTE,
                params![date, hour, minute, "input_keys", 1, 1],
            )?;
        }
        (EventType::Mouse, EventAction::Click) => {
            conn.execute(
                UPSERT_MINUTE,
                params![date, hour, minute, "input_clicks", 1, 1],
            )?;
        }
        (EventType::Window, EventAction::Switch) => {
            conn.execute(
                UPSERT_MINUTE,
                params![date, hour, minute, "window_switches", 1, 1],
            )?;
            if let Some(app) = e.app_name.as_deref() {
                conn.execute(UPSERT_DAILY_APP, params![date, app, 1])?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn json_counter(e: &Event, key: &str) -> i64 {
    e.event_data
        .as_ref()
        .and_then(|v| v.get(key))
        .and_then(|v| v.as_i64())
        .unwrap_or(0)
}

impl super::Database {
    /// 增量维护聚合缓存：对一批刚落库的事件做 agg_minute / agg_daily UPSERT。
    ///
    /// 由 writer 线程在每次 flush 后调用。失败仅 log（缓存可由 backfill 重建，
    /// 不影响原始写入）。
    pub fn update_agg(&self, events: &[Event]) {
        self.with_writer(
            |conn| {
                for e in events {
                    if let Err(err) = apply_event(conn, e) {
                        log::warn!("agg 缓存增量更新失败（跳过单条）: {err}");
                    }
                }
            },
            || log::warn!("agg 缓存增量更新跳过：写连接不可用"),
        );
    }

    /// 全量重建聚合读缓存（写连接上执行）。返回 agg_minute 行数，失败返回 0。
    /// 派生数据操作：events 原样不动。
    pub fn rebuild_agg(&self) -> usize {
        self.with_writer(
            |conn| {
                crate::db::agg::rebuild_all(conn).unwrap_or_else(|e| {
                    log::warn!("agg 缓存全量重建失败: {e}");
                    0
                })
            },
            || 0,
        )
    }
}

/// 从 events 全量重建聚合缓存（幂等）。返回重建的 agg_minute 行数。
///
/// 兼容两种事件形态：raw（press/click/switch 逐行）与 input_agg（计数 JSON）。
pub fn rebuild_all(conn: &Connection) -> crate::Result<usize> {
    let off = super::super::queries::local_offset_modifier();
    conn.execute("DELETE FROM agg_minute", [])?;
    conn.execute("DELETE FROM agg_daily WHERE bucket_id LIKE 'app:%'", [])?;

    // 分钟桶：一次 GROUP BY 本地分钟扫描，产出 4 类 bucket 行（单语句 UNION ALL，
    // 共享同一 CTE 扫描）。
    // input_agg 行的计数从 event_data JSON 提取；raw 行按行计数。
    conn.execute_batch(&format!(
        "INSERT INTO agg_minute (date, hour, minute, bucket_id, sum_value, count_value)
         WITH m AS (
           SELECT substr(datetime(timestamp, '{off}'), 1, 10) AS d,
                  CAST(substr(datetime(timestamp, '{off}'), 12, 2) AS INTEGER) AS h,
                  CAST(substr(datetime(timestamp, '{off}'), 15, 2) AS INTEGER) AS mi,
                  SUM({keys_row}) AS keys,
                  SUM({clicks_row}) AS clicks,
                  SUM(CASE WHEN event_type='mouse' AND event_action='input_agg' AND json_valid(event_data)
                           THEN COALESCE(json_extract(event_data, '$.move_distance_px'), 0) ELSE 0 END) AS dist,
                  SUM(CASE WHEN event_type='mouse' AND event_action='input_agg' AND json_valid(event_data)
                           THEN COALESCE(json_extract(event_data, '$.moves'), 0)
                           WHEN event_type IN ('keyboard','mouse') AND event_action <> 'input_agg'
                           THEN 1 ELSE 0 END) AS samples,
                  SUM(CASE WHEN event_type='window' AND event_action='switch' THEN 1 ELSE 0 END) AS switches
           FROM events
           WHERE event_type IN ('keyboard','mouse','window')
           GROUP BY d, h, mi
         ),
         b AS (
           SELECT d, h, mi, 'input_keys' AS bk, keys AS s, samples AS c FROM m WHERE keys > 0
           UNION ALL
           SELECT d, h, mi, 'input_clicks', clicks, samples FROM m WHERE keys > 0 OR clicks > 0
           UNION ALL
           SELECT d, h, mi, 'input_moves', dist, samples FROM m WHERE dist > 0
           UNION ALL
           SELECT d, h, mi, 'window_switches', switches, switches FROM m WHERE switches > 0
         )
         SELECT d, h, mi, bk, s, c FROM b;

         INSERT INTO agg_daily (date, bucket_id, sum_value, count_value)
         SELECT substr(datetime(timestamp, '{off}'), 1, 10),
                'app:' || app_name,
                NULL,
                COUNT(*)
         FROM events
         WHERE app_name IS NOT NULL AND event_type IN ('keyboard','mouse','window')
         GROUP BY 1, 2;",
        off = off,
        keys_row = crate::queries::KEYS_ROW_EXPR,
        clicks_row = crate::queries::CLICKS_ROW_EXPR,
    ))?;

    let n = conn.query_row("SELECT COUNT(*) FROM agg_minute", [], |r| r.get(0))?;
    Ok(n)
}

/// 懒回填：agg 全空而 events 非空时执行一次全量重建（存量库首开路径）。
/// 返回是否执行了回填。
pub fn backfill_if_needed(conn: &Connection) -> bool {
    let has_events: bool = conn
        .query_row("SELECT EXISTS(SELECT 1 FROM events)", [], |r| r.get(0))
        .unwrap_or(false);
    let has_agg: bool = conn
        .query_row("SELECT EXISTS(SELECT 1 FROM agg_minute)", [], |r| r.get(0))
        .unwrap_or(false);
    if !has_events || has_agg {
        return false;
    }
    match rebuild_all(conn) {
        Ok(n) => {
            log::info!("agg 缓存懒回填完成（{} 行 agg_minute）", n);
            true
        }
        Err(e) => {
            log::warn!("agg 缓存懒回填失败（get_anomalies 将回退 events 现算）: {e}");
            false
        }
    }
}

/// 某本地日期是否有 agg_minute 缓存行。表不存在 / 查询失败一律返回 false
/// （调用方回退 events 现算——保证聚合缺失时行为退化为原始慢路径而非错误）。
pub fn has_minute_for_date(conn: &Connection, date: &str) -> bool {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM agg_minute WHERE date = ?1)",
        params![date],
        |r| r.get(0),
    )
    .unwrap_or(false)
}

/// agg_daily 是否有 per-app 缓存行（可选限定某本地日期）。
pub fn has_app_daily(conn: &Connection, date: Option<&str>) -> bool {
    match date {
        Some(d) => conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM agg_daily WHERE date = ?1 AND bucket_id LIKE 'app:%')",
                params![d],
                |r| r.get(0),
            )
            .unwrap_or(false),
        None => conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM agg_daily WHERE bucket_id LIKE 'app:%')",
                [],
                |r| r.get(0),
            )
            .unwrap_or(false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(crate::db::SCHEMA).unwrap();
        crate::db::run_migrations(&c);
        c
    }

    fn ins(conn: &Connection, ts: &str, t: &str, a: &str, app: Option<&str>, data: Option<&str>) {
        conn.execute(
            "INSERT INTO events (timestamp, event_type, event_action, event_data, app_name, window_title, session_id)
             VALUES (?1,?2,?3,?4,?5,?6,1)",
            params![ts, t, a, data, app, app],
        )
        .unwrap();
    }

    /// UTC+8 固定偏移下"本地 2026-06-15 10:30"对应的 UTC RFC3339。
    /// 测试不依赖运行机器时区：datetime(ts,'+08:00 seconds') 是 SQL 端换算，
    /// local_offset_modifier 取运行机器——因此这里改用 UTC 机器无关断言：
    /// 直接断言重建后按本地日的总量正确，见 raw_preserved 与 minute_buckets。
    fn any_local_minute_row(conn: &Connection, bucket: &str) -> i64 {
        conn.query_row(
            "SELECT CAST(COALESCE(SUM(sum_value),0) AS INTEGER) FROM agg_minute WHERE bucket_id=?1",
            params![bucket],
            |r| r.get(0),
        )
        .unwrap()
    }

    #[test]
    fn rebuild_buckets_and_app_rows() {
        let c = conn();
        ins(
            &c,
            "2026-06-15T02:30:00+00:00",
            "keyboard",
            "press",
            None,
            None,
        );
        ins(
            &c,
            "2026-06-15T02:30:30+00:00",
            "keyboard",
            "press",
            None,
            None,
        );
        ins(
            &c,
            "2026-06-15T02:30:40+00:00",
            "mouse",
            "click",
            None,
            None,
        );
        ins(
            &c,
            "2026-06-15T02:31:00+00:00",
            "window",
            "switch",
            Some("code.exe"),
            None,
        );
        ins(
            &c,
            "2026-06-15T02:32:00+00:00",
            "keyboard",
            "input_agg",
            None,
            Some(r#"{"keys":7,"samples":7}"#),
        );

        let n = rebuild_all(&c).unwrap();
        assert!(n >= 3);
        // 总量与形态无关：2 条 raw press + 1 条 agg 行(7 keys)
        assert_eq!(any_local_minute_row(&c, "input_keys"), 9);
        assert_eq!(any_local_minute_row(&c, "input_clicks"), 1);
        assert_eq!(any_local_minute_row(&c, "window_switches"), 1);
        // app 日聚合：window 行带 app
        let app_cnt: i64 = c
            .query_row(
                "SELECT COALESCE(SUM(count_value),0) FROM agg_daily WHERE bucket_id='app:code.exe'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(app_cnt, 1);
        // 幂等：重建两次结果一致
        rebuild_all(&c).unwrap();
        assert_eq!(any_local_minute_row(&c, "input_keys"), 9);
    }

    /// 原始数据神圣性：重建/回填绝不改动 events 行（数量 + 内容全等）。
    #[test]
    fn raw_events_untouched_by_rebuild_and_backfill() {
        let c = conn();
        ins(
            &c,
            "2026-06-15T02:30:00+00:00",
            "keyboard",
            "press",
            None,
            None,
        );
        ins(
            &c,
            "2026-06-15T02:30:30+00:00",
            "mouse",
            "click",
            None,
            None,
        );
        ins(
            &c,
            "2026-06-15T03:00:00+00:00",
            "window",
            "switch",
            Some("word.exe"),
            Some(r#"{"hwnd":1}"#),
        );

        let snapshot = |c: &Connection| -> (i64, String) {
            let n: i64 = c
                .query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))
                .unwrap();
            // 内容指纹：全行拼接（含 id 顺序）
            let mut stmt = c
                .prepare("SELECT id||'|'||timestamp||'|'||event_type||'|'||event_action||'|'||COALESCE(event_data,'')||'|'||COALESCE(app_name,'')||'|'||COALESCE(window_title,'')||'|'||COALESCE(session_id,-1) FROM events ORDER BY id")
                .unwrap();
            let hash: String = stmt
                .query_map([], |r| r.get::<_, String>(0))
                .unwrap()
                .filter_map(|r| r.ok())
                .collect::<Vec<_>>()
                .join(";");
            (n, hash)
        };

        let before = snapshot(&c);
        assert!(backfill_if_needed(&c), "应有回填发生");
        rebuild_all(&c).unwrap();
        rebuild_all(&c).unwrap();
        let after = snapshot(&c);
        assert_eq!(before, after, "events 行数量与内容必须完全不变");
    }

    #[test]
    fn backfill_skipped_when_agg_present_or_events_empty() {
        let empty = conn();
        assert!(!backfill_if_needed(&empty), "空库不回填");

        let c = conn();
        ins(
            &c,
            "2026-06-15T02:30:00+00:00",
            "keyboard",
            "press",
            None,
            None,
        );
        assert!(backfill_if_needed(&c));
        // 已有 agg → 不再回填（增量维护负责新鲜度）
        assert!(!backfill_if_needed(&c));
    }

    #[test]
    fn incremental_update_matches_rebuild() {
        let c = conn();
        let e_press = Event::new(EventAction::Press, EventType::Keyboard);
        let mut e = e_press.clone();
        e.timestamp = "2026-06-15T02:30:10+00:00".into();
        let mut e2 = Event::new(EventAction::Switch, EventType::Window).app("code", "t");
        e2.timestamp = "2026-06-15T02:30:40+00:00".into();
        let mut e3 = Event::new(EventAction::InputAgg, EventType::Keyboard)
            .data(serde_json::json!({"keys": 5, "samples": 5}));
        e3.timestamp = "2026-06-15T02:31:00+00:00".into();

        for ev in [&e, &e2, &e3] {
            apply_event(&c, ev).unwrap();
            ins(
                &c,
                &ev.timestamp,
                ev.event_type.as_str(),
                ev.event_action.as_str(),
                ev.app_name.as_deref(),
                ev.event_data.as_ref().map(|v| v.to_string()).as_deref(),
            );
        }
        let incremental = any_local_minute_row(&c, "input_keys");

        rebuild_all(&c).unwrap();
        let rebuilt = any_local_minute_row(&c, "input_keys");
        assert_eq!(incremental, rebuilt, "增量维护必须与全量重建等价");
        assert_eq!(incremental, 6);
    }
}
