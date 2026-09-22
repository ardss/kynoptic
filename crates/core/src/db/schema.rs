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
         PRAGMA busy_timeout=5000;",
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
    heal_half_applied_0002(conn)?;
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
        let sql_effective: String =
            if *name == "0007_agg_minute_max_rowid" && agg_minute_has_max_event_rowid(conn) {
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

/// Wave22 P1：修复被 2026-09-09 之前的非事务版二进制"半应用"过 0002 的
/// 库——legacy_pet_signals 已建（改名成功）而 schema_version 卡在 2 以下，
/// 重跑 0002 的无条件 RENAME 会撞名硬失败且永远无法自愈。
/// 处置：pet_signals 若为空表（同次迁移的 CREATE IF NOT EXISTS 壳）则删壳
/// 让 RENAME 重放成功；若壳里有行则不动、报错指引用户（永不删数据）。
fn heal_half_applied_0002(conn: &Connection) -> rusqlite::Result<()> {
    // 审查 HIGH：命中条件改为**逐表判断**——旧条件"两张名字计数恰好 == 2"
    // 救不了 0002 中途断电最常见的三表残局（legacy_pet_signals + pet_memory
    // + pet_state 并存时改名已成功、pet_signals 壳已不在，COUNT=1≠2 → 判
    // skip → 重放 RENAME 撞名硬失败）。现在：legacy_pet_signals 存在且
    // pet_signals 也存在时才需要处置（空壳删掉让 RENAME 重放成功）。
    let legacy_exists: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master \
             WHERE type='table' AND name='legacy_pet_signals')",
            [],
            |r| r.get::<_, i64>(0),
        )
        .map(|n| n != 0)?;
    let shell_exists: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master \
             WHERE type='table' AND name='pet_signals')",
            [],
            |r| r.get::<_, i64>(0),
        )
        .map(|n| n != 0)?;
    if !(legacy_exists && shell_exists) {
        return Ok(());
    }
    let rows: i64 = conn.query_row(
        "SELECT COALESCE((SELECT COUNT(*) FROM pet_signals), 0)",
        [],
        |r| r.get(0),
    )?;
    if rows == 0 {
        conn.execute_batch("DROP TABLE IF EXISTS pet_signals;")?;
        log::info!("0002 自愈：移除空壳 pet_signals（legacy 归档已在位）");
    } else {
        // 审查 LOW：错误文本必须可行动——此前返回 InvalidColumnType(0,
        // "pet_signals", Null)，与真实原因（归档壳非空、需人工处置）无关。
        let msg = format!(
            "0002 自愈中止：pet_signals 归档壳含 {rows} 行数据，不能自动删除；\
             请人工确认并导出/迁移该表后删除 pet_signals，再重试打开数据库"
        );
        log::error!("{msg}");
        // ToSqlConversionFailure 的 Display 原样输出内层错误文本（无前缀包装），
        // 运维/用户看到的即是上面的可行动处置指引
        return Err(rusqlite::Error::ToSqlConversionFailure(Box::new(
            std::io::Error::other(msg),
        )));
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
