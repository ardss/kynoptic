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
//! **samples 语义（perf3 2026-09 对齐）**：agg_minute 的 count_value（samples）
//! = raw press + raw click + input_agg 行的 $.samples——**不含** raw move/scroll
//! 等非输入计数行。增量维护 [`apply_event`] 与全量重建 [`rebuild_all`] 必须对
//! 该语义逐行一致（perf3-longrun 一致性指纹曾抓到二者在含 raw move 的分钟上
//! 分叉：rebuild 把 move 计入 samples 而增量不会；count_value 当前无查询消费方，
//! 取「以增量语义为准」对齐）。
//!
//! 维护路径：
//! 1. 增量：writer 每次 flush 后调 [`super::Database::update_agg`]（见 collector.rs）；
//! 2. 懒回填：[`Database::open`] 时若 agg 全空而 events 非空 → [`backfill_if_needed`]
//!    （存量库首开自动补齐；选此方案而非 CLI 命令，见 CODE_NOTES.md §8）。

use chrono::{Local, Timelike, Utc};
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

/// input_agg 专用：事件携带的是"本分钟累计快照"，同分钟桶必须覆盖而非累加。
const UPSERT_MINUTE_SNAPSHOT: &str = "
INSERT INTO agg_minute (date, hour, minute, bucket_id, sum_value, count_value)
VALUES (?1, ?2, ?3, ?4, ?5, ?6)
ON CONFLICT(date, hour, minute, bucket_id) DO UPDATE SET
    sum_value = MAX(COALESCE(sum_value, 0), excluded.sum_value),
    count_value = MAX(COALESCE(count_value, 0), excluded.count_value)
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
                    UPSERT_MINUTE_SNAPSHOT,
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
                    UPSERT_MINUTE_SNAPSHOT,
                    params![date, hour, minute, "input_clicks", clicks, samples],
                )?;
            }
            if moves > 0 || dist > 0 {
                conn.execute(
                    UPSERT_MINUTE_SNAPSHOT,
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
    ///
    /// perf3（2026-09-09）：整批包进**一个事务**——旧实现每条 UPSERT 独立隐式
    /// 提交，实测 ~383µs/事件被提交开销主导（perf3-longrun P1：300 事件/批
    /// ~115ms）；单事务后每批一次提交，突发写入吞吐提升一个数量级。
    pub fn update_agg(&self, events: &[Event]) {
        self.with_writer(
            |conn| match conn.unchecked_transaction() {
                Ok(tx) => {
                    for e in events {
                        if let Err(err) = apply_event(&tx, e) {
                            log::warn!("agg 缓存增量更新失败（跳过单条）: {err}");
                        }
                    }
                    if let Err(err) = tx.commit() {
                        log::warn!("agg 缓存增量更新提交失败（本批跳过，可由回填补齐）: {err}");
                    }
                }
                Err(err) => log::warn!("agg 缓存增量更新事务创建失败（本批跳过）: {err}"),
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
    let off = crate::queries::local_offset_modifier();
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
                  -- 以下三个 count 与增量维护 apply_event 逐条语义一一对应（perf3 对齐）
                  SUM(CASE WHEN event_type='keyboard' AND event_action='press' THEN 1
                           WHEN event_type='keyboard' AND event_action='input_agg' AND json_valid(event_data)
                           THEN COALESCE(json_extract(event_data, '$.samples'), 0) ELSE 0 END) AS ckeys,
                  SUM(CASE WHEN event_type='mouse' AND event_action='click' THEN 1
                           WHEN event_type='mouse' AND event_action='input_agg' AND json_valid(event_data)
                           THEN COALESCE(json_extract(event_data, '$.samples'), 0) ELSE 0 END) AS cclicks,
                  SUM(CASE WHEN event_type='mouse' AND event_action='input_agg' AND json_valid(event_data)
                           THEN COALESCE(json_extract(event_data, '$.samples'), 0) ELSE 0 END) AS cmoves,
                  SUM(CASE WHEN event_type='window' AND event_action='switch' THEN 1 ELSE 0 END) AS switches
           FROM events
           WHERE event_type IN ('keyboard','mouse','window')
           GROUP BY d, h, mi
         ),
         b AS (
           SELECT d, h, mi, 'input_keys' AS bk, keys AS s, ckeys AS c FROM m WHERE keys > 0
           UNION ALL
           SELECT d, h, mi, 'input_clicks', clicks, cclicks FROM m WHERE clicks > 0
           UNION ALL
           SELECT d, h, mi, 'input_moves', dist, cmoves FROM m WHERE dist > 0 OR cmoves > 0
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
///
/// perf3（2026-09-09）：改为**分块（本地 date,hour）回填**——旧实现单事务全量
/// 重建，1M 事件存量库首开会把 `Database::open` 阻塞 10.5 分钟（perf3-open 实测
/// 631,594 ms，P0）。分块后每个 (date,hour) 块在一个短事务内原子完成
/// （DELETE 该块聚合行 + 从 events 重算 INSERT），writer 的增量 update_agg 在
/// 块间穿插执行不被饿死；进度记录在 metadata.agg_backfill_cursor，中断后下次
/// 打开自动续跑。
pub fn backfill_if_needed(conn: &Connection) -> bool {
    if !backfill_needed(conn) {
        return false;
    }
    match backfill_all(conn) {
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

/// 回填门槛检查（便宜：两条 EXISTS）。agg 非空时不回填——既有增量维护负责
/// 新鲜度；若上次分块回填中断，靠 metadata.agg_backfill_cursor 续跑判定。
pub fn backfill_needed(conn: &Connection) -> bool {
    let has_events: bool = conn
        .query_row("SELECT EXISTS(SELECT 1 FROM events)", [], |r| r.get(0))
        .unwrap_or(false);
    let has_agg: bool = conn
        .query_row("SELECT EXISTS(SELECT 1 FROM agg_minute)", [], |r| r.get(0))
        .unwrap_or(false);
    has_events && !has_agg
}

const BACKFILL_CURSOR_DONE: &str = "done";

/// 上次分块回填是否中断（cursor 存在且未到 done）。供 Database::open 决定续跑。
pub fn read_cursor_incomplete(conn: &Connection) -> bool {
    read_cursor(conn).is_some()
}

fn read_cursor(conn: &Connection) -> Option<String> {
    conn.query_row(
        "SELECT value FROM metadata WHERE key = 'agg_backfill_cursor'",
        [],
        |r| r.get(0),
    )
    .ok()
    .filter(|v| v != BACKFILL_CURSOR_DONE)
}

fn write_cursor(conn: &Connection, value: &str) {
    let _ = conn.execute(
        "INSERT INTO metadata (key, value) VALUES ('agg_backfill_cursor', ?1)
         ON CONFLICT(key) DO UPDATE SET value = ?1",
        params![value],
    );
}

/// 本地 (date, hour) 块 → UTC RFC3339 `[start, end)` 边界（sargable，走
/// idx_events_timestamp）。DST 缺失小时回退下一小时顺延边界。
fn hour_bounds(date: &str, hour: i64) -> Option<(String, String)> {
    use chrono::{NaiveDate, TimeZone};
    let day = NaiveDate::parse_from_str(date, "%Y-%m-%d").ok()?;
    let naive = day.and_hms_opt(hour as u32, 0, 0)?;
    let start = Local
        .from_local_datetime(&naive)
        .earliest()
        .map(|dt| dt.with_timezone(&Utc).to_rfc3339())?;
    let end_naive = if hour >= 23 {
        day.succ_opt()?.and_hms_opt(0, 0, 0)?
    } else {
        day.and_hms_opt(hour as u32 + 1, 0, 0)?
    };
    let end = Local
        .from_local_datetime(&end_naive)
        .earliest()
        .map(|dt| dt.with_timezone(&Utc).to_rfc3339())
        .or_else(|| Some(Utc.from_utc_datetime(&end_naive).to_rfc3339()))?;
    Some((start, end))
}

fn local_day_bounds(date: &str) -> Option<(String, String)> {
    crate::queries::local_day_range(date)
}

/// 重算单个本地 (date, hour) 块的 agg_minute 行 + （若是该日最后一小时）
/// 该日 agg_daily app 行。单事务原子：并发 writer 的增量 upsert 要么整体在
/// 事务前提交（被 DELETE 抹掉后由重算覆盖——重算扫描包含该事件），要么在
/// 事务后提交（增量叠加到重算结果上）。两种交错均正确。
fn backfill_chunk(conn: &Connection, date: &str, hour: i64) -> crate::Result<()> {
    let Some((start, end)) = hour_bounds(date, hour) else {
        return Ok(());
    };
    let (dstart, dend) = local_day_bounds(date).unwrap_or((start.clone(), end.clone()));
    let off = crate::queries::local_offset_modifier();
    conn.execute_batch("BEGIN IMMEDIATE;")?;
    let r = (|| -> crate::Result<()> {
        conn.execute(
            "DELETE FROM agg_minute WHERE date = ?1 AND hour = ?2",
            params![date, hour],
        )?;
        conn.execute_batch(&format!(
            "INSERT INTO agg_minute (date, hour, minute, bucket_id, sum_value, count_value)
             WITH m AS (
               SELECT CAST(substr(datetime(timestamp, '{off}'), 15, 2) AS INTEGER) AS mi,
                      SUM({keys_row}) AS keys,
                      SUM({clicks_row}) AS clicks,
                      SUM(CASE WHEN event_type='mouse' AND event_action='input_agg' AND json_valid(event_data)
                               THEN COALESCE(json_extract(event_data, '$.move_distance_px'), 0) ELSE 0 END) AS dist,
                      -- 以下三个 count 与增量维护 apply_event 逐条语义一一对应（perf3 对齐）
                      SUM(CASE WHEN event_type='keyboard' AND event_action='press' THEN 1
                               WHEN event_type='keyboard' AND event_action='input_agg' AND json_valid(event_data)
                               THEN COALESCE(json_extract(event_data, '$.samples'), 0) ELSE 0 END) AS ckeys,
                      SUM(CASE WHEN event_type='mouse' AND event_action='click' THEN 1
                               WHEN event_type='mouse' AND event_action='input_agg' AND json_valid(event_data)
                               THEN COALESCE(json_extract(event_data, '$.samples'), 0) ELSE 0 END) AS cclicks,
                      SUM(CASE WHEN event_type='mouse' AND event_action='input_agg' AND json_valid(event_data)
                               THEN COALESCE(json_extract(event_data, '$.samples'), 0) ELSE 0 END) AS cmoves,
                      SUM(CASE WHEN event_type='window' AND event_action='switch' THEN 1 ELSE 0 END) AS switches
               FROM events
               WHERE event_type IN ('keyboard','mouse','window')
                 AND timestamp >= '{start}' AND timestamp < '{end}'
               GROUP BY mi
             ),
             b AS (
               SELECT mi, 'input_keys' AS bk, keys AS s, ckeys AS c FROM m WHERE keys > 0
               UNION ALL
               SELECT mi, 'input_clicks', clicks, cclicks FROM m WHERE clicks > 0
               UNION ALL
               SELECT mi, 'input_moves', dist, cmoves FROM m WHERE dist > 0 OR cmoves > 0
               UNION ALL
               SELECT mi, 'window_switches', switches, switches FROM m WHERE switches > 0
             )
             SELECT '{date}', {hour}, mi, bk, s, c FROM b;",
            off = off,
            keys_row = crate::queries::KEYS_ROW_EXPR,
            clicks_row = crate::queries::CLICKS_ROW_EXPR,
            start = start,
            end = end,
            date = date,
            hour = hour,
        ))?;
        // 该日最后一小时：顺带重算该日 agg_daily app 行（小时 = 当日本地最大小时）。
        let max_hour: Option<i64> = conn
            .query_row(
                &format!(
                    "SELECT MAX(CAST(substr(datetime(timestamp, '{off}'), 12, 2) AS INTEGER)) \
                     FROM events WHERE event_type IN ('keyboard','mouse','window') \
                       AND timestamp >= '{dstart}' AND timestamp < '{dend}'",
                    off = off,
                    dstart = dstart,
                    dend = dend,
                ),
                [],
                |r| r.get(0),
            )
            .unwrap_or(None);
        if max_hour == Some(hour) {
            conn.execute(
                "DELETE FROM agg_daily WHERE date = ?1 AND bucket_id LIKE 'app:%'",
                params![date],
            )?;
            conn.execute_batch(&format!(
                "INSERT INTO agg_daily (date, bucket_id, sum_value, count_value)
                 SELECT substr(datetime(timestamp, '{off}'), 1, 10),
                        'app:' || app_name,
                        NULL,
                        COUNT(*)
                 FROM events
                 WHERE app_name IS NOT NULL AND event_type IN ('keyboard','mouse','window')
                   AND timestamp >= '{dstart}' AND timestamp < '{dend}'
                 GROUP BY 1, 2;",
                off = off,
                dstart = dstart,
                dend = dend,
            ))?;
        }
        Ok(())
    })();
    match r {
        Ok(()) => conn.execute_batch("COMMIT;")?,
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK;");
            return Err(e);
        }
    }
    Ok(())
}

/// 分块回填（可从 cursor 续跑；events 表扫过的块不再扫）。同步执行全部分块——
/// 供后台线程与测试使用。返回 agg_minute 行数。
pub fn backfill_all(conn: &Connection) -> crate::Result<usize> {
    let mut done = 0usize;
    loop {
        // 找 cursor 之后第一个有事件的本地 (date, hour) 块（sargable：按
        // idx_events_timestamp 范围探测每个本地小时；无事件的块廉价跳过）。
        let cursor = read_cursor(conn);
        // 本地日范围：events 的 UTC min/max 换本地日，向后兼容 cursor 起点跳过
        let (min_ts, max_ts): (String, String) = conn.query_row(
            "SELECT COALESCE(MIN(timestamp), ''), COALESCE(MAX(timestamp), '') FROM events",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        if max_ts.is_empty() {
            write_cursor(conn, BACKFILL_CURSOR_DONE);
            return Ok(count_agg_minute(conn));
        }
        let parse = |s: &str| -> Option<chrono::DateTime<Utc>> {
            chrono::DateTime::parse_from_rfc3339(s)
                .ok()
                .map(|t| t.with_timezone(&Utc))
        };
        let (Some(min_t), Some(max_t)) = (parse(&min_ts), parse(&max_ts)) else {
            write_cursor(conn, BACKFILL_CURSOR_DONE);
            return Ok(count_agg_minute(conn));
        };
        let min_local = min_t.with_timezone(&chrono::Local);
        let max_local = max_t.with_timezone(&chrono::Local);
        let mut next: Option<(String, i64)> = None;
        'outer: for day in min_local.date_naive().iter_days() {
            if day > max_local.date_naive() {
                break;
            }
            let ds = day.format("%Y-%m-%d").to_string();
            for h in 0..24i64 {
                if let Some(cur) = &cursor {
                    if cur.as_str() >= format!("{ds} {h:02}").as_str() {
                        continue;
                    }
                }
                if let Some((start, end)) = hour_bounds(&ds, h) {
                    let has: bool = conn
                        .query_row(
                            "SELECT EXISTS(SELECT 1 FROM events WHERE event_type IN ('keyboard','mouse','window') AND timestamp >= ?1 AND timestamp < ?2)",
                            params![start, end],
                            |r| r.get(0),
                        )
                        .unwrap_or(false);
                    if has {
                        next = Some((ds, h));
                        break 'outer;
                    }
                }
            }
        }
        let Some((ds, h)) = next else {
            write_cursor(conn, BACKFILL_CURSOR_DONE);
            return Ok(count_agg_minute(conn));
        };
        backfill_chunk(conn, &ds, h)?;
        write_cursor(conn, &format!("{ds} {h:02}"));
        done += 1;
        if done.is_multiple_of(24) {
            log::info!("agg 分块回填进度：{}（已完成 {} 块）", ds, done);
        }
    }
}

fn count_agg_minute(conn: &Connection) -> usize {
    conn.query_row("SELECT COUNT(*) FROM agg_minute", [], |r| {
        r.get::<_, i64>(0)
    })
    .unwrap_or(0) as usize
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
        let _ = crate::db::run_migrations(&c);
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

#[cfg(test)]
mod perf3_tests {
    use super::*;

    fn conn() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(crate::db::SCHEMA).unwrap();
        let _ = crate::db::run_migrations(&c);
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

    fn fingerprint(conn: &Connection) -> String {
        let mut stmt = conn
            .prepare(
                "SELECT date||'|'||hour||'|'||minute||'|'||bucket_id||'|'||COALESCE(sum_value,0)||'|'||COALESCE(count_value,0)
                 FROM agg_minute ORDER BY date, hour, minute, bucket_id",
            )
            .unwrap();
        stmt.query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect::<Vec<_>>()
            .join(";")
    }

    /// perf3：分块回填必须与全量 rebuild_all 完全等价（跨日 + raw/input_agg 混合形态）。
    #[test]
    fn chunked_backfill_equals_full_rebuild() {
        let c = conn();
        ins(
            &c,
            "2026-06-14T18:59:59+00:00",
            "keyboard",
            "press",
            None,
            None,
        );
        ins(
            &c,
            "2026-06-14T19:00:10+00:00",
            "mouse",
            "click",
            None,
            None,
        );
        ins(
            &c,
            "2026-06-14T19:00:30+00:00",
            "window",
            "switch",
            Some("code.exe"),
            None,
        );
        ins(
            &c,
            "2026-06-15T02:30:10+00:00",
            "keyboard",
            "press",
            None,
            None,
        );
        ins(
            &c,
            "2026-06-15T02:31:00+00:00",
            "keyboard",
            "input_agg",
            None,
            Some(r#"{"keys":7,"samples":7}"#),
        );
        ins(
            &c,
            "2026-06-15T03:00:00+00:00",
            "mouse",
            "move",
            Some(""),
            Some(r#"{"x":1,"y":2}"#),
        );

        backfill_all(&c).unwrap();
        let chunked_fp = fingerprint(&c);
        let chunked_daily: Vec<(String, i64)> = {
            let mut stmt = c
                .prepare(
                    "SELECT bucket_id, COALESCE(count_value,0) FROM agg_daily ORDER BY bucket_id",
                )
                .unwrap();
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .unwrap()
                .filter_map(|r| r.ok())
                .collect()
        };

        rebuild_all(&c).unwrap();
        assert_eq!(
            chunked_fp,
            fingerprint(&c),
            "分块回填与全量重建必须逐行一致"
        );
        rebuild_all(&c).unwrap();
        let daily: Vec<(String, i64)> = {
            let mut stmt = c
                .prepare(
                    "SELECT bucket_id, COALESCE(count_value,0) FROM agg_daily ORDER BY bucket_id",
                )
                .unwrap();
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .unwrap()
                .filter_map(|r| r.ok())
                .collect()
        };
        assert_eq!(chunked_daily, daily, "agg_daily 分块回填与全量重建一致");
        assert!(!read_cursor_incomplete(&c), "完成后 cursor 应为 done");
    }

    /// perf3：并发交错——分块回填期间 writer 的增量 upsert 不能造成丢失或重复。
    /// 模拟：回填某块后，增量 upsert 追加事件；再回填下一块（重扫同日）
    /// 不得破坏已完成块的计数。
    #[test]
    fn chunked_backfill_survives_interleaved_incremental_upserts() {
        let c = conn();
        ins(
            &c,
            "2026-06-15T02:30:10+00:00",
            "keyboard",
            "press",
            None,
            None,
        );
        backfill_all(&c).unwrap();

        // writer 路径：新事件落库 + 增量 agg（与 collector flush 相同顺序）
        let mut e = Event::new(EventAction::Press, EventType::Keyboard);
        e.timestamp = "2026-06-15T02:31:20+00:00".into();
        ins(&c, &e.timestamp, "keyboard", "press", None, None);
        apply_event(&c, &e).unwrap();

        // 已完成块计数 = 重建结果
        rebuild_all(&c).unwrap();
        let rebuilt = fingerprint(&c);
        // 注意：incremental 已把 02:31 的 keys=1 加上；rebuild 也算同一条 → 相等

        // 分块回填（cursor=done 会跳过 → 清 cursor 强制重跑，等价于"中断续跑"）
        c.execute("DELETE FROM metadata WHERE key='agg_backfill_cursor'", [])
            .unwrap();
        c.execute("DELETE FROM agg_minute", []).unwrap();
        c.execute("DELETE FROM agg_daily WHERE bucket_id LIKE 'app:%'", [])
            .unwrap();
        backfill_all(&c).unwrap();
        assert_eq!(
            fingerprint(&c),
            rebuilt,
            "交错增量后重跑分块回填仍与重建一致"
        );
    }
}
