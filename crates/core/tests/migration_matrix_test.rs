//! 迁移矩阵测试（审查清单 A3）：
//! 1. 对每个迁移 n（0002..000N），从 v(n-1) 的库状态逐级升级到该级成功；
//!    文件数与 constants::CURRENT_SCHEMA_VERSION 一致（防"加了文件忘了改常量"）。
//! 2. 事务回滚：迁移式事务（BEGIN IMMEDIATE → DDL → 失败 → ROLLBACK）后
//!    schema_version 不变、无半成品对象——run_migrations 依赖的正是这个模式。
//!
//! 注：MIGRATIONS 列表是 crate 私有常量，集成测试从源码目录直接读编号 SQL
//! 文件（cargo 测试 cwd=crate 根，另用 CARGO_MANIFEST_DIR 双保险）。

use rusqlite::Connection;
use std::path::PathBuf;

fn migrations_dir() -> PathBuf {
    let mut d = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    d.push("src");
    d.push("db");
    d.push("migrations");
    d
}

/// 排序后的迁移 SQL 文件（0001_init.sql ...），仅 .sql
fn migration_files() -> Vec<PathBuf> {
    let dir = migrations_dir();
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("读迁移目录 {} 失败: {e}", dir.display()))
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().map(|x| x == "sql").unwrap_or(false))
        .collect();
    files.sort();
    assert!(!files.is_empty(), "迁移目录为空");
    files
}

fn version_of(conn: &Connection) -> i64 {
    conn.query_row(
        "SELECT value FROM metadata WHERE key='schema_version'",
        [],
        |r| r.get::<_, String>(0),
    )
    .ok()
    .and_then(|s| s.parse().ok())
    .unwrap_or(0)
}

/// 新建裸库：应用 0001 并置 schema_version=1（等价 v1 存量库）
fn v1_conn() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(kynoptic_core::db::SCHEMA).unwrap();
    conn.execute(
        "INSERT INTO metadata (key, value) VALUES ('schema_version', '1')",
        [],
    )
    .unwrap();
    conn
}

/// 从 v1 开始，手工逐级应用 files[1..=k-1]（即 0002..=第k个文件），
/// 事务 + 版本号，与 run_migrations 相同模式。返回连接（版本 = k）。
fn upgraded_to(files: &[PathBuf], k: usize) -> Connection {
    assert!(k >= 1 && k <= files.len());
    let conn = v1_conn();
    for f in &files[1..k] {
        let sql = std::fs::read_to_string(f).unwrap();
        conn.execute_batch("BEGIN IMMEDIATE;").unwrap();
        conn.execute_batch(&sql).unwrap();
        let v = f
            .file_stem()
            .unwrap()
            .to_string_lossy()
            .split('_')
            .next()
            .unwrap()
            .trim_start_matches('0')
            .to_string();
        let v: i64 = if v.is_empty() { 0 } else { v.parse().unwrap() };
        conn.execute(
            "INSERT INTO metadata (key, value) VALUES ('schema_version', ?1)
             ON CONFLICT(key) DO UPDATE SET value = ?1",
            [v.to_string()],
        )
        .unwrap();
        conn.execute_batch("COMMIT;").unwrap();
    }
    conn
}

#[test]
fn migration_file_count_matches_current_schema_version() {
    let n = migration_files().len() as i64;
    assert_eq!(
        n,
        kynoptic_core::constants::CURRENT_SCHEMA_VERSION,
        "迁移文件数与 CURRENT_SCHEMA_VERSION 不一致——新增迁移文件后必须同步常量"
    );
}

/// 矩阵主体：对每个 k（2..=N），先手工升到 v(k-1)，再交给 run_migrations
/// 完成剩余升级——必须成功且最终版本 = CURRENT_SCHEMA_VERSION。
#[test]
fn every_migration_step_upgrades_successfully_from_previous_version() {
    let files = migration_files();
    let n = files.len();
    for k in 2..=n {
        let conn = upgraded_to(&files, k - 1);
        assert_eq!(
            version_of(&conn),
            (k - 1) as i64,
            "前置：应停在 v{}",
            k - 1
        );
        kynoptic_core::db::run_migrations(&conn)
            .unwrap_or_else(|e| panic!("从 v{} 升级失败: {e}", k - 1));
        assert_eq!(
            version_of(&conn),
            kynoptic_core::constants::CURRENT_SCHEMA_VERSION,
            "从 v{} 出发未到达最终版本",
            k - 1
        );
        // 每条路径终点都应具备完整 bucket 模型表
        for table in ["agg_minute", "agg_daily", "schema_meta", "current_state"] {
            let ok: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
                    [table],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(ok, 1, "从 v{} 升级后缺少表 {}", k - 1, table);
        }
    }
}

/// 中途打断的迁移事务必须整体回滚：schema_version 不变、无半成品表、
/// 原有数据原样。run_migrations 的失败路径就是「ROLLBACK + 硬失败」。
#[test]
fn failed_migration_transaction_rolls_back_and_keeps_version() {
    let conn = v1_conn();
    let v_before = version_of(&conn);
    let events_before: i64 = conn
        .query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))
        .unwrap();

    // 模拟一次中途失败的迁移事务：合法 DDL 先执行，随后坏 SQL 报错
    conn.execute_batch("BEGIN IMMEDIATE;").unwrap();
    let r = conn.execute_batch(
        "CREATE TABLE half_done_migration (id INTEGER PRIMARY KEY);
         INSERT INTO events (timestamp, event_type, event_action) VALUES ('x','y','z');
         SELECT * FROM table_that_does_not_exist;",
    );
    assert!(r.is_err(), "坏 SQL 应该报错");
    conn.execute_batch("ROLLBACK;").unwrap();

    assert_eq!(version_of(&conn), v_before, "回滚后 schema_version 必须不变");
    let half: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='half_done_migration'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(half, 0, "回滚后不得残留半成品表");
    let events_after: i64 = conn
        .query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))
        .unwrap();
    assert_eq!(events_before, events_after, "回滚后 events 不得残留脏行");

    // 回滚后的库必须仍可正常完成全部迁移
    kynoptic_core::db::run_migrations(&conn).expect("回滚后的库应能完成迁移");
    assert_eq!(version_of(&conn), kynoptic_core::constants::CURRENT_SCHEMA_VERSION);
}
