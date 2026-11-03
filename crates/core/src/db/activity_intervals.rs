//! activity_intervals 派生区间表（PRD v1.0 跨生态活动感知，Stage 1，migration 0014）
//!
//! 统一时间轴的区间数据来自本表；它是**只读派生缓存**（stage0-audit §3：不进
//! 真相链），任何时刻都可由 events 全量重建（[`rebuild_activity_intervals_at`]），
//! 因此 UI / CLI / MCP 三端只读本表即可保证数值一致（PRD DoD）。
//!
//! 重建口径（stage0-audit §2/§4）：
//! - foreground_dwell：按 window/switch 事件推导 (app_name, window_title) 去重
//!   区间。**连续同 (app, title) 只刷新 end、不产生新区间**——修复旧口径
//!   「相邻 switch 事件相减」被重复事件多算时长的问题（样本实测同题 1 秒内
//!   最多 3 条、每分钟整点一次刷新触发）。
//! - away：idle_start/idle_end 事件对推导；连续 idle_start 以新的为准
//!   （与 db/sessions.rs 同语义，丢一段无终点的计时、不猜）；尾部未闭合按
//!   重建时刻收口。
//! - quality：observed = 证据闭环；estimated = 区间与 away 相交（前台在场
//!   存疑，输入空闲但焦点仍在）——**只打标不裁剪**：estimated 段的
//!   duration_ms 含 away 内整段时长（stage0-audit §4 的「∩ 在场区间」按
//!   相交标记口径实现，不做净时长裁剪）；unknown = 断线收口（末段无后续
//!   switch 佐证，按重建时刻 now 收口——unknown 是真实缺口，不是 0）。
//! - tab_foreground_dwell：Stage 2 F02 扩展（tab_change 证据）接入后启用，
//!   当前 rebuild 不产出该 kind。

use rusqlite::{params, Connection};

/// 区间种类：前台驻留
pub const KIND_FOREGROUND_DWELL: &str = "foreground_dwell";
/// 区间种类：浏览器 tab 前台驻留（Stage 2 预留）
pub const KIND_TAB_FOREGROUND_DWELL: &str = "tab_foreground_dwell";
/// 区间种类：离场
pub const KIND_AWAY: &str = "away";

/// production 入口：真实时钟 → 注入式收口。
pub fn rebuild_activity_intervals(conn: &Connection) -> rusqlite::Result<usize> {
    rebuild_activity_intervals_at(conn, chrono::Utc::now().timestamp_millis())
}

/// 注入时钟版全量重建（`now_ms` = UTC epoch 毫秒）：测试零真实时钟依赖
/// （红线「测试不得读真实时钟」），生产行为与 [`rebuild_activity_intervals`]
/// 完全一致。返回写入的区间行数。
pub fn rebuild_activity_intervals_at(conn: &Connection, now_ms: i64) -> rusqlite::Result<usize> {
    let switches = load_switches(conn)?;
    let away = derive_away_intervals(conn, now_ms);
    let dwell = derive_dwell_intervals(&switches, now_ms, &away);

    conn.execute_batch("BEGIN IMMEDIATE")?;
    let outcome = (|| -> rusqlite::Result<usize> {
        // 派生表：先清后建，全量口径，无增量状态
        conn.execute("DELETE FROM activity_intervals", [])?;
        let mut stmt = conn.prepare(
            "INSERT INTO activity_intervals \
             (interval_kind, start_ts, end_ts, app_name, title_hash, quality, source_id) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        )?;
        let mut n = 0usize;
        for iv in &dwell {
            n += stmt.execute(params![
                KIND_FOREGROUND_DWELL,
                rfc3339(iv.start_ms),
                rfc3339(iv.end_ms),
                iv.app,
                title_hash(&iv.title),
                iv.quality,
                iv.source.as_deref().unwrap_or("windows"),
            ])?;
        }
        for iv in &away {
            n += stmt.execute(params![
                KIND_AWAY,
                rfc3339(iv.start_ms),
                rfc3339(iv.end_ms),
                Option::<String>::None,
                Option::<String>::None,
                iv.quality,
                "windows",
            ])?;
        }
        Ok(n)
    })();
    match outcome {
        Ok(n) => {
            conn.execute_batch("COMMIT")?;
            Ok(n)
        }
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(e)
        }
    }
}

// ─── 区间推导（纯函数，测试直接覆盖）───────────────────────────────────────

#[derive(Debug, PartialEq)]
struct Interval {
    start_ms: i64,
    end_ms: i64,
    quality: &'static str,
}

#[derive(Debug, PartialEq)]
struct DwellInterval {
    start_ms: i64,
    end_ms: i64,
    app: String,
    title: String,
    quality: &'static str,
    source: Option<String>,
}

struct RawSwitch {
    ts_ms: i64,
    app: String,
    title: String,
    source: Option<String>,
}

/// window/switch 事件按时间序读出（app_name 为空的行不参与前台区间推导）。
fn load_switches(conn: &Connection) -> rusqlite::Result<Vec<RawSwitch>> {
    let mut stmt = conn.prepare(
        "SELECT timestamp, app_name, window_title, source_id FROM events \
         WHERE event_type = 'window' AND event_action = 'switch' \
           AND app_name IS NOT NULL AND app_name != '' \
         ORDER BY timestamp, id",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, Option<String>>(2)?,
            r.get::<_, Option<String>>(3)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (ts, app, title, source) = row?;
        if let Ok(t) = chrono::DateTime::parse_from_rfc3339(&ts) {
            out.push(RawSwitch {
                ts_ms: t.timestamp_millis(),
                app,
                title: title.unwrap_or_default(),
                source,
            });
        }
    }
    Ok(out)
}

/// away 区间：idle_start/idle_end 事件对。
fn derive_away_intervals(conn: &Connection, now_ms: i64) -> Vec<Interval> {
    let mut stmt = match conn.prepare(
        "SELECT timestamp, event_action FROM events \
         WHERE event_action IN ('idle_start', 'idle_end') ORDER BY timestamp, id",
    ) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let rows: Vec<(i64, String)> = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        .map(|it| {
            it.filter_map(|r| r.ok())
                .filter_map(|(ts, action)| {
                    chrono::DateTime::parse_from_rfc3339(&ts)
                        .ok()
                        .map(|t| (t.timestamp_millis(), action))
                })
                .collect()
        })
        .unwrap_or_default();

    let mut out = Vec::new();
    let mut open: Option<i64> = None;
    for (ts, action) in rows {
        match action.as_str() {
            // 连续两个 idle_start：以新的为准（丢一段无终点的计时，不猜）
            "idle_start" => open = Some(ts),
            "idle_end" => {
                if let Some(start) = open.take() {
                    out.push(Interval {
                        start_ms: start,
                        end_ms: ts,
                        quality: "observed",
                    });
                }
            }
            _ => {}
        }
    }
    // 尾部未闭合：按重建时刻收口（unknown——无 idle_end 佐证）
    if let Some(start) = open {
        out.push(Interval {
            start_ms: start,
            end_ms: now_ms,
            quality: "unknown",
        });
    }
    out
}

/// foreground_dwell 去重区间：连续同 (app, title) 只刷新 end；不同 key 关闭
/// 当前区间并开新段。区间与 away **相交则标 estimated（只打标，不裁剪
/// duration）**——duration_ms 仍是完整去重区间时长，estimated 段含义是
/// 「该时长内在场存疑」，而非被 away 排除后的净时长；下游不得把 estimated
/// 段当作确定在场直接求和。尾部断线 → 按 now 收口标 unknown。
fn derive_dwell_intervals(
    switches: &[RawSwitch],
    now_ms: i64,
    away: &[Interval],
) -> Vec<DwellInterval> {
    let overlaps_away = |start_ms: i64, end_ms: i64| -> bool {
        away.iter()
            .any(|a| start_ms < a.end_ms && a.start_ms < end_ms)
    };
    let mut out: Vec<DwellInterval> = Vec::new();
    for s in switches {
        match out.last_mut() {
            // 连续同 (app, title)：只刷新，不产生新区间（重复事件去重口径）
            Some(cur) if cur.app == s.app && cur.title == s.title => {
                if s.ts_ms > cur.end_ms {
                    cur.end_ms = s.ts_ms;
                }
            }
            _ => {
                // 开新段前，把当前段按 away 相交补标记（out 非空时 last 已闭合）
                if let Some(cur) = out.last_mut() {
                    if cur.quality != "unknown" && overlaps_away(cur.start_ms, cur.end_ms) {
                        cur.quality = "estimated";
                    }
                }
                out.push(DwellInterval {
                    start_ms: s.ts_ms,
                    end_ms: s.ts_ms,
                    app: s.app.clone(),
                    title: s.title.clone(),
                    quality: "observed",
                    source: s.source.clone(),
                });
            }
        }
    }
    // 尾部段：断线收口（无后续 switch 佐证）→ unknown + now；否则按 away 相交标记
    if let Some(cur) = out.last_mut() {
        if cur.end_ms < now_ms && !switches.is_empty() {
            let is_tail = switches
                .last()
                .map(|s| s.ts_ms == cur.end_ms && s.app == cur.app && s.title == cur.title)
                .unwrap_or(false);
            if is_tail {
                cur.end_ms = now_ms.max(cur.end_ms);
                cur.quality = "unknown";
            } else if overlaps_away(cur.start_ms, cur.end_ms) {
                cur.quality = "estimated";
            }
        } else if overlaps_away(cur.start_ms, cur.end_ms) {
            cur.quality = "estimated";
        }
    }
    out
}

/// window_title → 6 位十六进制短哈希（样本脱敏口径 title_h，如 9c2545）。
/// 哈希仅用于派生表分组合并展示，原始 title 仍在 events 表（唯一真相源）。
///
/// 算法 = FNV-1a 64-bit，**显式固定**（此前用 std `DefaultHasher`/SipHash，
/// Rust 官方不保证其跨版本稳定；本值持久化在 activity_intervals 表中，故
/// 换成不受 rustc 版本影响的显式算法，增量重建/跨版本对比不再漂移）。
fn title_hash(title: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325; // FNV offset basis
    for b in title.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3); // FNV prime
    }
    format!("{:06x}", h & 0xFF_FFFF)
}

fn rfc3339(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .unwrap_or(chrono::DateTime::UNIX_EPOCH)
        .to_rfc3339()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    /// 库连接：内存库每测试单独 open + 迁移，互不共享。
    fn migrated_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(crate::db::SCHEMA).unwrap();
        crate::db::run_migrations(&conn).unwrap();
        conn
    }

    /// 插入一条 window/switch 事件（ts 为相对 base 的秒偏移，统一 UTC RFC3339）。
    const BASE_MS: i64 = 1_792_977_600_000; // 2026-11-02T03:00:00Z，固定不读真实时钟

    fn ts(base_ms: i64, offset_secs: i64) -> String {
        rfc3339(base_ms + offset_secs * 1000)
    }

    fn insert_switch(conn: &Connection, base_ms: i64, offset_secs: i64, app: &str, title: &str) {
        conn.execute(
            "INSERT INTO events (timestamp, event_type, event_action, app_name, window_title) \
             VALUES (?1, 'window', 'switch', ?2, ?3)",
            params![ts(base_ms, offset_secs), app, title],
        )
        .unwrap();
    }

    fn insert_idle(conn: &Connection, base_ms: i64, offset_secs: i64, action: &str) {
        conn.execute(
            "INSERT INTO events (timestamp, event_type, event_action) VALUES (?1, 'system', ?2)",
            params![ts(base_ms, offset_secs), action],
        )
        .unwrap();
    }

    fn count_kind(conn: &Connection, kind: &str) -> i64 {
        conn.query_row(
            "SELECT COUNT(*) FROM activity_intervals WHERE interval_kind = ?1",
            params![kind],
            |r| r.get(0),
        )
        .unwrap()
    }

    fn dwell_secs(conn: &Connection, app: &str, title: &str) -> Vec<(i64, String)> {
        let mut stmt = conn
            .prepare(
                "SELECT (julianday(end_ts) - julianday(start_ts)) * 86400.0, quality \
                 FROM activity_intervals \
                 WHERE interval_kind='foreground_dwell' AND app_name=?1 AND title_hash=?2 \
                 ORDER BY start_ts",
            )
            .unwrap();
        // title_hash 与实现同源：直接查回存好的行
        let th = title_hash(title);
        stmt.query_map(params![app, th], |r| {
            Ok((
                (r.get::<_, f64>(0)? * 1000.0).round() as i64,
                r.get::<_, String>(1)?,
            ))
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
    }

    fn away_rows(conn: &Connection) -> Vec<(i64, String)> {
        let mut stmt = conn
            .prepare(
                "SELECT (julianday(end_ts) - julianday(start_ts)) * 86400.0, quality \
                 FROM activity_intervals WHERE interval_kind='away' ORDER BY start_ts",
            )
            .unwrap();
        stmt.query_map([], |r| {
            Ok((
                (r.get::<_, f64>(0)? * 1000.0).round() as i64,
                r.get::<_, String>(1)?,
            ))
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
    }

    /// 重复事件去重口径（stage0-audit §2）：同 (app,title) 连续刷新只产生
    /// 一个区间，时长 = 末次刷新 − 首次出现（相邻相减的旧口径会多算）。
    #[test]
    fn duplicate_same_title_events_produce_single_interval() {
        let conn = migrated_conn();
        // 0s 首现，1s/3s 同题重复，30s 每分钟整点刷新（样本真实模式）
        for off in [0, 1, 3, 30] {
            insert_switch(&conn, BASE_MS, off, "WindowsTerminal.exe", "doc");
        }
        let now = BASE_MS + 60_000;
        let n = rebuild_activity_intervals_at(&conn, now).unwrap();
        assert_eq!(count_kind(&conn, KIND_FOREGROUND_DWELL), 1);
        let rows = dwell_secs(&conn, "WindowsTerminal.exe", "doc");
        assert_eq!(rows.len(), 1);
        // 时长按断线收口 = now(60s) − 首现(0s)（末段无 switch 佐证，缺口如实入账）
        assert_eq!(rows[0].0, 60_000);
        assert_eq!(rows[0].1, "unknown"); // 断线收口：无后续 switch，按 now 收口
        assert_eq!(n, 1);
    }

    /// sample-30min.json 真实模式 fixture（11:00-11:02 段构造）：来回切换、
    /// 同题重复、每分钟整点刷新。断言区间数与时长符合 §4 新口径。
    #[test]
    fn sample_real_pattern_intervals_match_new_semantics() {
        let conn = migrated_conn();
        let switches: &[(i64, &str, &str)] = &[
            (0, "WindowsTerminal.exe", "9c2545"),
            (6, "chrome.exe", "c3f5fd"),
            (60, "WindowsTerminal.exe", "9c2545"),
            (63, "WindowsTerminal.exe", "9c2545"),
            (64, "WindowsTerminal.exe", "9c2545"),
            (64, "WindowsTerminal.exe", "fc0431"),
            (68, "WindowsTerminal.exe", "a1f52c"),
            (71, "WindowsTerminal.exe", "a1f52c"),
            (76, "WindowsTerminal.exe", "a1f52c"),
            (78, "WindowsTerminal.exe", "fc0431"),
            (79, "WindowsTerminal.exe", "fc0431"),
            (120, "WindowsTerminal.exe", "9c2545"), // 每分钟整点刷新
            (126, "chrome.exe", "c3f5fd"),
        ];
        for (off, app, title) in switches {
            insert_switch(&conn, BASE_MS, *off, app, title);
        }
        let now = BASE_MS + 130_000;
        rebuild_activity_intervals_at(&conn, now).unwrap();

        // 8 段连续同题 run（每段 = 相邻不同 (app,title) 边界切一次）
        assert_eq!(count_kind(&conn, KIND_FOREGROUND_DWELL), 8);

        // a1f52c 段：3 条同题重复 → 1 个区间 8s（旧口径相邻相减也是 8s，
        // 但 fc0431 双条 78/79 段旧口径会把中间重复算成 0 加上多算 1s；
        // 9c2545 整点刷新段 60-64 是本口径修复点）
        let a1 = dwell_secs(&conn, "WindowsTerminal.exe", "a1f52c");
        assert_eq!(a1.len(), 1);
        assert_eq!(a1[0].0, 8_000);
        assert_eq!(a1[0].1, "observed");

        // 9c2545 整点刷新段 60→64 = 4s
        let wt = dwell_secs(&conn, "WindowsTerminal.exe", "9c2545");
        assert_eq!(wt.len(), 3);
        assert_eq!(wt[1].0, 4_000);

        // 尾部 chrome：断线收口，end = now（130s），quality=unknown
        let chrome = dwell_secs(&conn, "chrome.exe", "c3f5fd");
        assert_eq!(chrome.len(), 2);
        assert_eq!(chrome[1].0, 4_000); // 126s → now(130s)
        assert_eq!(chrome[1].1, "unknown");
    }

    /// idle 交集口径（§4：foreground_dwell = 去重区间 ∩ 在场区间）：
    /// away 覆盖期间的前台区间标 estimated（焦点在但无输入，在场存疑）。
    #[test]
    fn dwell_overlapping_away_is_marked_estimated() {
        let conn = migrated_conn();
        insert_switch(&conn, BASE_MS, 0, "app.exe", "A");
        insert_switch(&conn, BASE_MS, 15, "app.exe", "A"); // 同题刷新 → A 段 [0,15s] 跨 away 起点
        insert_idle(&conn, BASE_MS, 10, "idle_start");
        insert_switch(&conn, BASE_MS, 20, "app.exe", "B"); // away 期间切窗（无输入证据）
        insert_switch(&conn, BASE_MS, 35, "app.exe", "B"); // 同题刷新 → B 段 [20,35s]
        insert_idle(&conn, BASE_MS, 40, "idle_end");
        insert_switch(&conn, BASE_MS, 50, "app.exe", "A");
        let now = BASE_MS + 60_000;
        rebuild_activity_intervals_at(&conn, now).unwrap();

        // away 区间完整落在库中
        let away = away_rows(&conn);
        assert_eq!(away.len(), 1);
        assert_eq!(away[0].0, 30_000);
        assert_eq!(away[0].1, "observed");

        // A 段 [0,15s] 与 away[10,40] 相交 → estimated
        let a = dwell_secs(&conn, "app.exe", "A");
        assert_eq!(a.len(), 2); // [0,15s] 与 [50s,now] 尾段
        assert_eq!(a[0].0, 15_000);
        assert_eq!(a[0].1, "estimated");
        assert_eq!(a[1].0, 10_000);
        assert_eq!(a[1].1, "unknown"); // [50s,60s] 尾段，断线收口优先

        // B 段 [20,35s] 整体落在 away 内 → estimated
        let b = dwell_secs(&conn, "app.exe", "B");
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].0, 15_000);
        assert_eq!(b[0].1, "estimated");
    }

    /// 断线收口（§2 样本 11:17:48 后零证据缺口）：尾部区间按重建时刻收口、
    /// 标 unknown——unknown 是真实缺口，不允许静默归 0。
    #[test]
    fn trailing_interval_is_closed_at_now_as_unknown() {
        let conn = migrated_conn();
        insert_switch(&conn, BASE_MS, 0, "app.exe", "only");
        let now = BASE_MS + 90_000;
        rebuild_activity_intervals_at(&conn, now).unwrap();
        let rows = dwell_secs(&conn, "app.exe", "only");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, 90_000); // 0s → now(90s)
        assert_eq!(rows[0].1, "unknown");
    }

    /// 幂等：重建两遍行集一致（派生表先清后建，无增量状态）。
    #[test]
    fn rebuild_is_idempotent() {
        let conn = migrated_conn();
        insert_switch(&conn, BASE_MS, 0, "app.exe", "x");
        insert_switch(&conn, BASE_MS, 5, "app.exe", "x");
        let now = BASE_MS + 30_000;
        let n1 = rebuild_activity_intervals_at(&conn, now).unwrap();
        let n2 = rebuild_activity_intervals_at(&conn, now).unwrap();
        assert_eq!(n1, n2);
        assert_eq!(count_kind(&conn, KIND_FOREGROUND_DWELL), 1);
        assert_eq!(
            count_kind(&conn, KIND_TAB_FOREGROUND_DWELL),
            0,
            "tab_foreground_dwell 为 Stage 2 预留，当前不产出"
        );
    }

    /// migration 0014 存量回填：迁移前插入的旧事件在升级后
    /// source_id='windows'、quality='observed'（append-only：行数不变）。
    #[test]
    fn migration_0014_backfills_legacy_events() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(crate::db::SCHEMA).unwrap();
        // v1 存量库：先插入旧结构事件，再升级
        conn.execute(
            "INSERT INTO events (timestamp, event_type, event_action, app_name) \
             VALUES ('2026-11-01T10:00:00+00:00', 'window', 'switch', 'a.exe')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO metadata (key, value) VALUES ('schema_version', '1')",
            [],
        )
        .unwrap();
        crate::db::run_migrations(&conn).unwrap();
        let (total, with_source, with_quality): (i64, i64, i64) = conn
            .query_row(
                "SELECT COUNT(*), \
                 SUM(source_id='windows'), \
                 SUM(quality='observed') FROM events",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(total, 1);
        assert_eq!(with_source, 1, "存量行应回填 source_id='windows'");
        assert_eq!(with_quality, 1, "存量行应回填 quality='observed'");
        // 新写入事件不再被 UPDATE 覆盖显式值（幂等守卫 WHERE source_id IS NULL）
        conn.execute(
            "INSERT INTO events (timestamp, event_type, event_action, source_id, quality) \
             VALUES ('2026-11-01T10:01:00+00:00', 'window', 'switch', 'browser', 'confirmed')",
            [],
        )
        .unwrap();
        let kept: String = conn
            .query_row(
                "SELECT source_id FROM events WHERE timestamp='2026-11-01T10:01:00+00:00'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(kept, "browser", "显式值不得被回填覆盖");
    }
}
