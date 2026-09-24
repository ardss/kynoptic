//! 数据库 Schema：编号 SQL 迁移（唯一事实源）、PRAGMA 配置
//!
//! 迁移系统：`migrations/` 目录下的编号 SQL 文件（0001_init.sql、0002_bucket_model.sql）
//! 是所有 DDL 的唯一来源。每个文件在一个事务内执行（成功后记录版本号，失败整体回滚），
//! 与《schema-ddl-draft-v1》"migrations 唯一事实源（编号 SQL，事务执行）"一致。
//!
//! - 0001：v0.1 采集器基础表（events / sessions / metadata / daily_agg + 索引）
//! - 0002：开放 bucket 模型（schema_meta / buckets / event_types / agg_minute /
//!   agg_daily / current_state）+ pet 遗留表清理
//! - 0010：device_snapshot「最新含字段快照」部分索引（api_input / overview 硬件卡）
//!
//! PRAGMA 只在 [`apply_pragmas`] 出现一次。

use rusqlite::{params, Connection};

/// 迁移文件列表（文件名即版本号，按序执行）。
///
/// 用 include_str! 编译期嵌入，运行期无需依赖文件系统布局。
const MIGRATIONS: &[(&str, &str)] = &[
    ("0001_init", include_str!("migrations/0001_init.sql")),
    (
        "0002_bucket_model",
        include_str!("migrations/0002_bucket_model.sql"),
    ),
    (
        "0003_perf_indexes",
        include_str!("migrations/0003_perf_indexes.sql"),
    ),
    (
        "0004_perf3_indexes",
        include_str!("migrations/0004_perf3_indexes.sql"),
    ),
    (
        "0005_input_agg_upsert",
        include_str!("migrations/0005_input_agg_upsert.sql"),
    ),
    (
        "0006_post_v5_repair",
        include_str!("migrations/0006_post_v5_repair.sql"),
    ),
    (
        "0007_agg_minute_max_rowid",
        include_str!("migrations/0007_agg_minute_max_rowid.sql"),
    ),
    (
        "0008_events_action_idx",
        include_str!("migrations/0008_events_action_idx.sql"),
    ),
    (
        "0009_events_action_ts",
        include_str!("migrations/0009_events_action_ts.sql"),
    ),
    (
        "0010_snapshot_partial_idx",
        include_str!("migrations/0010_snapshot_partial_idx.sql"),
    ),
];

/// 0001 的内容单独导出：供 CLI 等外部工具对裸库做幂等初始化。
pub const SCHEMA: &str = include_str!("migrations/0001_init.sql");

/// PRAGMA 配置的唯一来源（WAL + 调优）。
pub fn apply_pragmas(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "PRAGMA journal_mode=WAL;
         PRAGMA synchronous=NORMAL;
         PRAGMA cache_size=-8000;
         PRAGMA temp_store=MEMORY;
         PRAGMA mmap_size=268435456;
         PRAGMA busy_timeout=5000;
         -- WAL 高水位封顶（perf 审查 2026-09）：两次维护间隔内 checkpoint
         -- TRUNCATE 可能因常驻 reader busy 而截断失败，journal_size_limit
         -- 让 WAL 在其后下一笔写入回落到 16MB（实测不影响写入吞吐）。
         PRAGMA journal_size_limit=16777216;",
    )
}

/// 只读连接的 PRAGMA 子集（MCP 工具面 open_reader 用）。
///
/// 审查 P1：`journal_mode=WAL` 是**写操作**，在 SQLITE_OPEN_READ_ONLY 连接上
/// 对任何尚未处于 WAL 的库都会失败（attempt to write a readonly database），
/// 导致该库上所有工具报 "Failed to open database"。此处只保留纯读安全的
/// 调优项（cache_size/temp_store/mmap_size/busy_timeout），
/// 跳过 journal_mode 与 synchronous（后者随 journal_mode 一起无意义）。
pub fn apply_pragmas_readonly(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "PRAGMA cache_size=-8000;
         PRAGMA temp_store=MEMORY;
         PRAGMA mmap_size=268435456;
         PRAGMA busy_timeout=5000;",
    )
}

/// 读取当前 schema 版本（0 表示全新库）。
///
/// 审查 MEDIUM：schema_version 只存 metadata 文本、解析失败静默归零会触发
/// 全量重放（0002 无条件 RENAME / 0007 裸 ALTER 不幂等，重放即撞名硬失败）。
/// 现在权威版本同步写入 `PRAGMA user_version`（整数、库文件内、外部工具可见）：
/// 优先读 user_version；为 0（老库尚未写入）时回退 metadata 并把解析失败
/// 显式告警（不再静默）。
fn current_version(conn: &Connection) -> i64 {
    let uv: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap_or(0);
    if uv > 0 {
        return uv;
    }
    match conn
        .query_row(
            "SELECT value FROM metadata WHERE key = 'schema_version'",
            [],
            |r| r.get::<_, String>(0),
        )
        .ok()
        .and_then(|s| s.parse::<i64>().ok())
    {
        Some(v) => v,
        None => {
            // metadata 有值但解析失败（格式漂移/损坏）：告警后按 0 处理，
            // 由 run_migrations 的守卫决定是否安全重放
            let raw: Option<String> = conn
                .query_row(
                    "SELECT value FROM metadata WHERE key = 'schema_version'",
                    [],
                    |r| r.get(0),
                )
                .ok();
            if let Some(raw) = raw {
                log::warn!("metadata.schema_version 值异常（{raw:?}），按 0 处理");
            }
            0
        }
    }
}

/// 把权威版本同步写入 PRAGMA user_version（写失败不阻塞——metadata 仍是
/// 可用回退源，只是外部工具短暂看不到准确版本）。
fn sync_user_version(conn: &Connection, version: i64) {
    let _ = conn.execute_batch(&format!("PRAGMA user_version = {version};"));
}

/// 审查 HIGH：0007 是唯一非 IF NOT EXISTS 迁移（裸 ALTER ADD COLUMN）。
/// 非事务老二进制可能"加列成功但没写 schema_version"（断电），重启重放
/// 即 duplicate column 且无自愈路径。列已存在时跳过 ALTER、只补记版本号。
fn agg_minute_has_max_event_rowid(conn: &Connection) -> bool {
    let mut stmt = match conn.prepare("PRAGMA table_info(agg_minute)") {
        Ok(s) => s,
        Err(_) => return false,
    };
    let rows = stmt.query_map([], |r| r.get::<_, String>(1));
    match rows {
        Ok(it) => it
            .filter_map(|r| r.ok())
            .any(|name| name == "max_event_rowid"),
        Err(_) => false,
    }
}

/// 执行全部未应用的编号迁移。每个迁移在独立事务内执行：
/// 成功后写入 schema_version；任一迁移失败则回滚并**硬失败**（审查 P1：
/// 旧版吞错会让库带病运行——schema_version 卡住导致下次启动重跑迁移
/// 再失败，或唯一索引缺失使 input_agg UPSERT 全链路静默归零）。
pub fn run_migrations(conn: &Connection) -> rusqlite::Result<()> {
    let mut applied = current_version(conn);
    // 审查 MEDIUM：未来版本库守卫——schema_version 高于本二进制已知迁移数
    // 说明库被更新版本的程序迁移过，旧代码静默零操作后会按旧列集向新库
    // 写入。拒绝打开（硬失败）而不是带病运行。
    if applied > MIGRATIONS.len() as i64 {
        let msg = format!(
            "数据库 schema 版本 (v{applied}) 高于本程序支持的版本 (v{})，拒绝以旧代码打开未来版本库",
            MIGRATIONS.len()
        );
        log::error!("{msg}");
        return Err(rusqlite::Error::InvalidParameterName(msg));
    }
    for (idx, (name, sql)) in MIGRATIONS.iter().enumerate() {
        let version = (idx + 1) as i64;
        if applied >= version {
            continue;
        }
        // 0007 半应用自愈（审查 HIGH）：max_event_rowid 列已存在（非事务
        // 老二进制加列成功但未写版本）时跳过 ALTER，只补记版本号。
        // 0002 半应用自愈（审查 33-F2，重构）：pet 段从 SQL 剥离，改在 Rust
        // 侧按 sqlite_master 逐表条件执行（见 apply_0002_pet_tables）——SQL
        // 侧「缺则建空壳 + 无条件 RENAME」在 legacy_pet_signals 已在场的任何
        // 半应用残局下重放必撞名硬失败，库永久打不开。
        let is_0002 = *name == "0002_bucket_model";
        let sql_effective: String = if is_0002 {
            match sql.find("-- pet 遗留表改名保留") {
                Some(i) => format!("-- {name}: pet 段改由 Rust 侧条件执行\n{}", &sql[..i]),
                None => (*sql).to_string(),
            }
        } else if *name == "0007_agg_minute_max_rowid" && agg_minute_has_max_event_rowid(conn) {
            log::warn!(
                "0007 自愈：agg_minute.max_event_rowid 已存在（半应用），跳过 ALTER 只补记版本号"
            );
            format!("-- {name}: 列已存在，跳过（半应用自愈）")
        } else {
            (*sql).to_string()
        };
        conn.execute_batch("BEGIN IMMEDIATE;")?;
        let outcome = conn
            .execute_batch(&sql_effective)
            .and_then(|_| {
                if is_0002 {
                    apply_0002_pet_tables(conn)
                } else {
                    Ok(())
                }
            })
            .and_then(|_| {
                conn.execute(
                    "INSERT INTO metadata (key, value) VALUES ('schema_version', ?1)
                     ON CONFLICT(key) DO UPDATE SET value = ?1",
                    params![version.to_string()],
                )
            })
            .and_then(|_| conn.execute_batch("COMMIT;"));
        if let Err(e) = outcome {
            let _ = conn.execute_batch("ROLLBACK;");
            log::error!("迁移 {name} 失败，数据库保持原版本: {e}");
            return Err(e);
        }
        log::info!("已应用迁移 {name} (v{version})");
        applied = version;
    }
    // 权威版本同步到 PRAGMA user_version（审查 MEDIUM：单靠 metadata 文本
    // 脆弱，user_version 整数存于库文件、外部工具可见）
    sync_user_version(conn, applied);
    Ok(())
}

/// 0002 的 pet 遗留段：Rust 侧按 sqlite_master **逐表条件执行**（审查 33-F2
/// 重构，取代旧 heal_half_applied_0002 + SQL「缺则建空壳 + 无条件 RENAME」）。
///
/// 旧实现的重放死路（第 33 轮审查实测 S3a/S3b 两种残局均复现）：只要
/// legacy_pet_signals 已存在（改名成功而版本号未写，即半应用），重放 0002
/// 的 `ALTER TABLE pet_signals RENAME TO legacy_pet_signals` 必撞名硬失败，
/// 且每次 Database::open 都重跑同一路径——库永久打不开；heal 删空壳处置
/// 对象也错了（冲突是 legacy 目标名已占用，不是壳）。
///
/// 逐表语义：
/// - 目标 legacy_* 已在 → 跳过（该表已改名归档）；若同名源 pet_* 还在且
///   **有数据**（异常残局：归档与现表并存）→ 报错指引用户，永不删数据；
/// - 源 pet_* 存在 → RENAME 归档（含残局中漏改的 pet_memory/pet_state）；
/// - 两者皆无（全新库）→ 建空壳再改名，保留「新库留下空 legacy_* 归档表」
///   的既有语义。
fn apply_0002_pet_tables(conn: &Connection) -> rusqlite::Result<()> {
    const PET_TABLES: [(&str, &str); 3] = [
        ("pet_signals", "legacy_pet_signals"),
        ("pet_memory", "legacy_pet_memory"),
        ("pet_state", "legacy_pet_state"),
    ];
    let table_exists = |name: &str| -> rusqlite::Result<bool> {
        conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
            params![name],
            |r| r.get::<_, i64>(0),
        )
        .map(|n| n != 0)
    };
    for (src, dst) in PET_TABLES {
        if table_exists(dst)? {
            if table_exists(src)? {
                let rows: i64 =
                    conn.query_row(&format!("SELECT COUNT(*) FROM {src}"), [], |r| r.get(0))?;
                if rows > 0 {
                    let msg = format!(
                        "0002 自愈中止：{src} 含 {rows} 行数据且 {dst} 归档已在位\
                         （异常残局），不能自动处置；请人工确认并合并/导出后重试"
                    );
                    log::error!("{msg}");
                    return Err(rusqlite::Error::ToSqlConversionFailure(Box::new(
                        std::io::Error::other(msg),
                    )));
                }
                // 空壳（半应用重放时 CREATE IF NOT EXISTS 留下）且归档已在位：
                // 删壳无损（零行数据），避免 pet_* 现表残留
                conn.execute_batch(&format!("DROP TABLE {src};"))?;
            }
            continue;
        }
        if table_exists(src)? {
            conn.execute_batch(&format!("ALTER TABLE {src} RENAME TO {dst};"))?;
        } else {
            conn.execute_batch(&format!(
                "CREATE TABLE {src} (id INTEGER PRIMARY KEY);
                 ALTER TABLE {src} RENAME TO {dst};"
            ))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constants;

    /// 全新内存库：迁移应全部成功且版本号到位；再跑一遍应幂等。
    #[test]
    fn migrations_apply_and_are_idempotent() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        let _ = run_migrations(&conn);
        assert_eq!(current_version(&conn), constants::CURRENT_SCHEMA_VERSION);

        // 幂等：重复执行不报错、版本不变
        let _ = run_migrations(&conn);
        assert_eq!(current_version(&conn), constants::CURRENT_SCHEMA_VERSION);

        // bucket 模型表存在
        for table in [
            "schema_meta",
            "buckets",
            "event_types",
            "agg_minute",
            "agg_daily",
            "current_state",
        ] {
            let n: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
                    [table],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(n, 1, "缺少表 {table}");
        }

        // pet 遗留表已被清理
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name LIKE 'pet%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 0, "pet 表应已清理");
    }

    /// v1（仅 0001 已应用）升级路径：0002 应补齐 bucket 模型表并清掉 pet 遗留表。
    #[test]
    fn migration_from_v1_drops_pet_tables_and_fills_bucket_model() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        conn.execute_batch(
            "CREATE TABLE pet_state (id INTEGER PRIMARY KEY);
             CREATE TABLE pet_memory (id INTEGER PRIMARY KEY);
             INSERT INTO metadata (key, value) VALUES ('schema_version', '1');",
        )
        .unwrap();
        let _ = run_migrations(&conn);
        assert_eq!(current_version(&conn), constants::CURRENT_SCHEMA_VERSION);
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name LIKE 'pet%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 0);
    }

    /// 审查 33-F2 回归：0002 半应用残局（legacy 已在 + metadata<2）必须能
    /// 自愈打开。S3a：三表残局（legacy + pet_memory + pet_state、壳不在）；
    /// S3b：legacy + 空壳。旧实现的「缺则建壳 + 无条件 RENAME」两种形态都
    /// 撞名硬失败且每次 open 重跑、库永久打不开。
    #[test]
    fn migration_0002_half_applied_heals_legacy_conflicts() {
        for (label, setup) in [
            (
                "S3a 三表残局",
                "CREATE TABLE legacy_pet_signals (id INTEGER PRIMARY KEY);
                 CREATE TABLE pet_memory (id INTEGER PRIMARY KEY);
                 CREATE TABLE pet_state (id INTEGER PRIMARY KEY);",
            ),
            (
                "S3b legacy+空壳",
                "CREATE TABLE legacy_pet_signals (id INTEGER PRIMARY KEY);
                 CREATE TABLE pet_signals (id INTEGER PRIMARY KEY);",
            ),
        ] {
            let conn = Connection::open_in_memory().unwrap();
            conn.execute_batch(SCHEMA).unwrap();
            conn.execute_batch(setup).unwrap();
            conn.execute_batch("INSERT INTO metadata (key, value) VALUES ('schema_version', '1');")
                .unwrap();
            run_migrations(&conn).unwrap_or_else(|e| panic!("{label}: 迁移失败: {e}"));
            assert_eq!(
                current_version(&conn),
                constants::CURRENT_SCHEMA_VERSION,
                "{label}: 版本必须推进到位"
            );
            // 残局源表已归档、无 pet_* 现表残留（legacy_* 归档保留）
            let n: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='table' \
                     AND (name = 'pet_signals' OR name = 'pet_memory' OR name = 'pet_state')",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(n, 0, "{label}: pet 现表应已全部改名归档");
            // 二次 open 幂等（旧实现卡 v1 死循环的路径）
            run_migrations(&conn).unwrap();
        }
    }

    /// 0010 部分索引命中验证（perf P0：api_input / overview 硬件卡的
    /// "最新含字段快照"查询，无匹配行时不得反向扫全部历史 device_snapshot）。
    /// 用 EXPLAIN QUERY PLAN 断言 dash 侧两条真实 SQL（WHERE 谓词必须与索引
    /// 谓词完全同形）走 idx_events_action_id_input / idx_events_action_id_hw。
    #[test]
    fn snapshot_partial_indexes_are_used_by_latest_snapshot_queries() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        let _ = run_migrations(&conn);

        let plan = |sql: &str| -> String {
            let mut stmt = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
            let mut out = String::new();
            let mut rows = stmt.query([]).unwrap();
            while let Some(r) = rows.next().unwrap() {
                out.push_str(&r.get::<_, String>(3).unwrap());
                out.push_str("; ");
            }
            out
        };

        // dash /api/input：最新 input_devices 拓扑快照
        let p = plan(
            "SELECT event_data FROM events \
             WHERE event_action = 'device_snapshot' \
               AND json_extract(event_data, '$.input_devices') IS NOT NULL \
             ORDER BY id DESC LIMIT 1",
        );
        assert!(
            p.contains("idx_events_action_id_input"),
            "api_input 快照查询应命中部分索引，实际计划: {p}"
        );

        // dash overview 硬件卡：最新 memory.total_gb 快照
        let p = plan(
            "SELECT event_data FROM events \
             WHERE event_action = 'device_snapshot' \
               AND json_extract(event_data, '$.memory.total_gb') IS NOT NULL \
             ORDER BY id DESC LIMIT 1",
        );
        assert!(
            p.contains("idx_events_action_id_hw"),
            "overview 硬件卡快照查询应命中部分索引，实际计划: {p}"
        );

        // 无匹配行时不退化：空表/无含字段行时计划不变（部分索引为空即 O(1)）
        conn.execute(
            "INSERT INTO events (timestamp, event_type, event_action, session_id) \
             VALUES ('2026-06-15T00:00:00+00:00', 'device', 'device_snapshot', 1)",
            [],
        )
        .unwrap();
        let p = plan(
            "SELECT event_data FROM events \
             WHERE event_action = 'device_snapshot' \
               AND json_extract(event_data, '$.input_devices') IS NOT NULL \
             ORDER BY id DESC LIMIT 1",
        );
        assert!(
            p.contains("idx_events_action_id_input"),
            "无匹配行时仍应走部分索引（空扫），实际计划: {p}"
        );
    }
}
