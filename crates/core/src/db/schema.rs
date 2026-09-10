//! 数据库 Schema：编号 SQL 迁移（唯一事实源）、PRAGMA 配置
//!
//! 迁移系统：`migrations/` 目录下的编号 SQL 文件（0001_init.sql、0002_bucket_model.sql）
//! 是所有 DDL 的唯一来源。每个文件在一个事务内执行（成功后记录版本号，失败整体回滚），
//! 与《schema-ddl-draft-v1》"migrations 唯一事实源（编号 SQL，事务执行）"一致。
//!
//! - 0001：v0.1 采集器基础表（events / sessions / metadata / daily_agg + 索引）
//! - 0002：开放 bucket 模型（schema_meta / buckets / event_types / agg_minute /
//!   agg_daily / current_state）+ pet 遗留表清理
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

/// 读取当前 schema 版本（metadata.schema_version，0 表示全新库）。
fn current_version(conn: &Connection) -> i64 {
    conn.query_row(
        "SELECT value FROM metadata WHERE key = 'schema_version'",
        [],
        |r| r.get::<_, String>(0),
    )
    .ok()
    .and_then(|s| s.parse().ok())
    .unwrap_or(0)
}

/// 执行全部未应用的编号迁移。每个迁移在独立事务内执行：
/// 成功后写入 schema_version；任一迁移失败则回滚并**硬失败**（审查 P1：
/// 旧版吞错会让库带病运行——schema_version 卡住导致下次启动重跑迁移
/// 再失败，或唯一索引缺失使 input_agg UPSERT 全链路静默归零）。
pub fn run_migrations(conn: &Connection) -> rusqlite::Result<()> {
    let mut applied = current_version(conn);
    for (idx, (name, sql)) in MIGRATIONS.iter().enumerate() {
        let version = (idx + 1) as i64;
        if applied >= version {
            continue;
        }
        conn.execute_batch("BEGIN IMMEDIATE;")?;
        let outcome = conn
            .execute_batch(sql)
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
}
