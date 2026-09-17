//! kynoptic-aggrepair —— agg_minute 受污染聚合缓存修复工具（独立一次性 bin）。
//!
//! 背景：2026-09-10 / 09-11（本地日）的 agg_minute input_keys sum 虚高约 20 倍
//! （旧双计数 bug 残留）。events 原始表完整可信、一字节不动；agg_* 是纯派生
//! 缓存，允许重算。
//!
//! 原则：
//! - 不自己写任何聚合 SQL：重算一律调用 kynoptic-core 公开的
//!   `kynoptic_core::db::agg::rebuild_all`（从 events 全量重建，确定性、幂等）。
//! - 「只重算受影响日期范围」：apply 前把范围外的 agg_minute / agg_daily(app:%)
//!   行逐行快照，rebuild_all 后原样插回——范围外聚合字节不变。
//! - 默认 dry-run：只读目标库 + 在临时副本上重算出期望值做对比，不写目标库。
//!   `--apply` 才在目标库上执行（单事务 BEGIN IMMEDIATE 包裹，原子提交），
//!   重算后自动用「新副本重算」复验并打印 PASS/FAIL。
//!
//! 用法：
//!   kynoptic-aggrepair --db D:/Kynoptic/data/kynoptic.db \
//!       --from 2026-09-10 --to 2026-09-11 [--apply]
//!
//! 日期为本地日、含端点。临时副本保留在 %TEMP%/kynoptic-aggrepair-* 备查。

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::exit;

use chrono::{NaiveDate, Utc};
use kynoptic_core::db::agg;
use rusqlite::{Connection, OpenFlags};

const BUCKET_KEYS: &str = "input_keys";

struct Args {
    db: PathBuf,
    from: String,
    to: String,
    apply: bool,
}

fn parse_args() -> Args {
    let mut db: Option<PathBuf> = None;
    let mut from: Option<String> = None;
    let mut to: Option<String> = None;
    let mut apply = false;
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--db" => db = it.next().map(PathBuf::from),
            "--from" => from = it.next(),
            "--to" => to = it.next(),
            "--apply" => apply = true,
            "--dry-run" => apply = false,
            "--help" | "-h" => {
                println!("usage: kynoptic-aggrepair --db PATH --from YYYY-MM-DD --to YYYY-MM-DD [--apply]");
                println!("  默认 dry-run：仅对比统计，不修改目标库。--apply 才执行重算。");
                exit(0);
            }
            other => {
                eprintln!("未知参数: {other}（见 --help）");
                exit(2);
            }
        }
    }
    let (Some(db), Some(from), Some(to)) = (db, from, to) else {
        eprintln!("缺少必填参数，需要 --db --from --to（见 --help）");
        exit(2);
    };
    for d in [&from, &to] {
        if NaiveDate::parse_from_str(d, "%Y-%m-%d").is_err() {
            eprintln!("日期格式错误: {d}（应为 YYYY-MM-DD 本地日）");
            exit(2);
        }
    }
    if from > to {
        eprintln!("--from 晚于 --to");
        exit(2);
    }
    Args {
        db,
        from,
        to,
        apply,
    }
}

/// 复制 db 三件套（db / -wal / -shm）到 %TEMP%/kynoptic-aggrepair-<tag>/，
/// 返回副本 db 路径。服务在写也能复制：副本由 SQLite 打开时用 wal 恢复到
/// 复制时刻的快照。
fn copy_db(src: &Path, tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("kynoptic-aggrepair-{tag}"));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("创建临时目录失败");
    let dst = dir.join("kynoptic.db");
    fs::copy(src, &dst).unwrap_or_else(|e| panic!("复制 {:?} 失败: {e}", src));
    let src_str = src.to_string_lossy().into_owned();
    for suffix in ["-wal", "-shm"] {
        let s = PathBuf::from(format!("{src_str}{suffix}"));
        if s.exists() {
            let d = PathBuf::from(format!("{}{suffix}", dst.to_string_lossy()));
            fs::copy(&s, &d).unwrap_or_else(|e| panic!("复制 {:?} 失败: {e}", s));
        }
    }
    dst
}

/// 读某日期范围内 agg_minute input_keys 的每分钟 sum_value：
/// (date, hour, minute) -> sum。只读 SQL（无聚合，逐行取回）。
fn read_minute_keys(conn: &Connection, from: &str, to: &str) -> BTreeMap<(String, i64, i64), f64> {
    let mut stmt = conn
        .prepare(&format!(
            "SELECT date, hour, minute, sum_value FROM agg_minute \
             WHERE date >= '{from}' AND date <= '{to}' AND bucket_id = '{BUCKET_KEYS}' \
             ORDER BY date, hour, minute",
            from = from,
            to = to,
            BUCKET_KEYS = BUCKET_KEYS,
        ))
        .expect("查询 agg_minute 失败");
    let mut map = BTreeMap::new();
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, f64>(3)?,
            ))
        })
        .expect("查询 agg_minute 失败");
    for row in rows {
        let (d, h, m, s) = row.expect("读取 agg_minute 行失败");
        map.insert((d, h, m), s);
    }
    map
}

/// 范围内逐分钟对比：返回差异分钟列表（key, 当前值, 期望值）。
fn diff_minutes(
    current: &BTreeMap<(String, i64, i64), f64>,
    expected: &BTreeMap<(String, i64, i64), f64>,
) -> Vec<((String, i64, i64), f64, f64)> {
    let mut keys: Vec<_> = current.keys().chain(expected.keys()).collect();
    keys.sort();
    keys.dedup();
    keys.into_iter()
        .filter_map(|k| {
            let c = current.get(k).copied().unwrap_or(0.0);
            let e = expected.get(k).copied().unwrap_or(0.0);
            // 值为 REAL；按 1e-6 绝对容差视为相等
            if (c - e).abs() > 1e-6 {
                Some((k.clone(), c, e))
            } else {
                None
            }
        })
        .collect()
}

fn print_compare(
    title: &str,
    from: &str,
    to: &str,
    current: &BTreeMap<(String, i64, i64), f64>,
    expected: &BTreeMap<(String, i64, i64), f64>,
) {
    println!("== {title} ==");
    let mut day = from.to_string();
    let end = to.to_string();
    loop {
        let cur_total: f64 = current
            .iter()
            .filter(|((d, _, _), _)| *d == day)
            .map(|(_, v)| *v)
            .sum();
        let exp_total: f64 = expected
            .iter()
            .filter(|((d, _, _), _)| *d == day)
            .map(|(_, v)| *v)
            .sum();
        let diffs = diff_minutes(
            &current
                .iter()
                .filter(|((d, _, _), _)| *d == day)
                .map(|(k, v)| (k.clone(), *v))
                .collect(),
            &expected
                .iter()
                .filter(|((d, _, _), _)| *d == day)
                .map(|(k, v)| (k.clone(), *v))
                .collect(),
        );
        let ratio = if exp_total > 0.0 {
            cur_total / exp_total
        } else {
            f64::NAN
        };
        println!(
            "  {} : 当前 keys 合计 = {:.0}, 期望(events 重算) = {:.0}, 差异分钟数 = {}{}",
            day,
            cur_total,
            exp_total,
            diffs.len(),
            if exp_total > 0.0 {
                format!(", 当前/期望 = {:.2}x", ratio)
            } else {
                String::new()
            },
        );
        for ((d, h, m), c, e) in diffs.iter().take(20) {
            println!(
                "    {} {:02}:{:02}  当前 = {:.0}, 期望 = {:.0}",
                d, h, m, c, e
            );
        }
        if diffs.len() > 20 {
            println!("    ...（其余 {} 个差异分钟省略）", diffs.len() - 20);
        }
        if day == end {
            break;
        }
        let nd = NaiveDate::parse_from_str(&day, "%Y-%m-%d")
            .expect("日期")
            .succ_opt()
            .expect("日期")
            .format("%Y-%m-%d")
            .to_string();
        day = nd;
    }
}

/// SQLite 文件头魔数（前 16 字节）。
const SQLITE_HEADER: &[u8; 16] = b"SQLite format 3\0";

/// 文件头是否为 SQLite 3 数据库。纯函数，单测覆盖。
fn is_sqlite_header(head: &[u8]) -> bool {
    head.len() >= SQLITE_HEADER.len() && &head[..SQLITE_HEADER.len()] == SQLITE_HEADER
}

/// 前置校验：--db 必须是可读的 SQLite 文件。修复"非 SQLite 文件先复制后
/// panic"——复制一个几 GB 的非库文件纯属浪费，且 panic 信息不可读。
/// 1) 文件头 16 字节魔数；2) 只读打开 + PRAGMA quick_check。
///
/// 任一失败：友好报错并 exit 2，绝不进入复制/重算流程。
fn preflight_db_check(db: &Path) {
    if !db.is_file() {
        eprintln!("错误: {:?} 不是文件（不存在或是目录）", db);
        exit(2);
    }
    let mut head = [0u8; 16];
    match fs::File::open(db).and_then(|mut f| {
        use std::io::Read;
        f.read_exact(&mut head)
    }) {
        Ok(()) => {}
        Err(e) => {
            eprintln!("错误: 无法读取 {:?} 的文件头: {e}", db);
            exit(2);
        }
    }
    if !is_sqlite_header(&head) {
        eprintln!(
            "错误: {:?} 不是 SQLite 数据库文件（文件头魔数不符）。确认 --db 指向 kynoptic.db 后重试。",
            db
        );
        exit(2);
    }
    let conn = Connection::open_with_flags(
        db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )
    .unwrap_or_else(|e| {
        eprintln!("错误: 只读打开 {:?} 失败: {e}", db);
        exit(2);
    });
    let quick: String = conn
        .query_row("PRAGMA quick_check", [], |r| r.get(0))
        .unwrap_or_else(|e| {
            eprintln!("错误: PRAGMA quick_check 执行失败: {e}");
            exit(2);
        });
    if quick != "ok" {
        eprintln!(
            "错误: {:?} 完整性检查未通过（quick_check: {quick}）。先修复数据库再重算。",
            db
        );
        exit(2);
    }
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    let args = parse_args();
    println!(
        "kynoptic-aggrepair: db = {:?}, 范围 = {} ..= {}（本地日，含端点）, 模式 = {}",
        args.db,
        args.from,
        args.to,
        if args.apply { "APPLY" } else { "DRY-RUN" }
    );

    // ---- 0. 前置校验：非 SQLite / 损坏库直接 exit 2，不做任何复制 ----
    preflight_db_check(&args.db);

    // ---- 1. 期望值：副本上用公开 rebuild_all 从 events 全量重算（只读原始数据）----
    let copy1 = copy_db(&args.db, "expect");
    {
        let conn = Connection::open(&copy1).expect("打开副本失败");
        kynoptic_core::db::run_migrations(&conn).expect("副本迁移失败");
        let t0 = std::time::Instant::now();
        let n = agg::rebuild_all(&conn).expect("副本重算失败");
        println!(
            "副本重算完成：{} 行 agg_minute（{} ms）",
            n,
            t0.elapsed().as_millis()
        );
    }
    let expected_conn = Connection::open(&copy1).expect("重开副本失败");
    let expected = read_minute_keys(&expected_conn, &args.from, &args.to);

    // ---- 2. 目标库当前值（只读连接）----
    let target_ro = Connection::open_with_flags(
        &args.db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )
    .unwrap_or_else(|e| panic!("只读打开 {:?} 失败: {e}", args.db));
    let current = read_minute_keys(&target_ro, &args.from, &args.to);

    print_compare(
        "dry-run 对比（agg 当前值 vs events 重算期望值）",
        &args.from,
        &args.to,
        &current,
        &expected,
    );
    let total_diff = diff_minutes(&current, &expected).len();
    println!("范围差异分钟总数 = {}", total_diff);
    if !args.apply {
        println!("DRY-RUN 结束：未修改目标库。确认无误后加 --apply 执行。");
        return;
    }

    // ---- 3. apply：范围外快照 -> 事务内 rebuild_all -> 快照插回 ----
    println!("--apply：开始重算目标库（范围外聚合行将原样保留）...");
    let target =
        Connection::open(&args.db).unwrap_or_else(|e| panic!("打开 {:?} 失败: {e}", args.db));
    kynoptic_core::db::run_migrations(&target).expect("目标库迁移失败");

    // 快照范围外 agg_minute 与 agg_daily(app:%) 行（逐行读入内存）
    #[allow(dead_code)]
    struct MinRow {
        date: String,
        hour: i64,
        minute: i64,
        bucket: String,
        sum: Option<f64>,
        count: Option<i64>,
        max_rowid: i64,
    }
    #[allow(dead_code)]
    struct DayRow {
        date: String,
        bucket: String,
        sum: Option<f64>,
        count: Option<i64>,
    }
    let mut min_rows = Vec::new();
    {
        let mut stmt = target
            .prepare(&format!(
                "SELECT date, hour, minute, bucket_id, sum_value, count_value, COALESCE(max_event_rowid, 0) FROM agg_minute \
                 WHERE NOT (date >= '{}' AND date <= '{}')",
                args.from, args.to
            ))
            .expect("快照 agg_minute 失败");
        let rows = stmt
            .query_map([], |r| {
                Ok(MinRow {
                    date: r.get(0)?,
                    hour: r.get(1)?,
                    minute: r.get(2)?,
                    bucket: r.get(3)?,
                    sum: r.get(4)?,
                    count: r.get(5)?,
                    max_rowid: r.get(6)?,
                })
            })
            .expect("快照 agg_minute 失败");
        for row in rows {
            min_rows.push(row.expect("快照 agg_minute 行失败"));
        }
    }
    let mut day_rows = Vec::new();
    {
        let mut stmt = target
            .prepare("SELECT date, bucket_id, sum_value, count_value FROM agg_daily WHERE bucket_id LIKE 'app:%'")
            .expect("快照 agg_daily 失败");
        let rows = stmt
            .query_map([], |r| {
                Ok(DayRow {
                    date: r.get(0)?,
                    bucket: r.get(1)?,
                    sum: r.get(2)?,
                    count: r.get(3)?,
                })
            })
            .expect("快照 agg_daily 失败");
        for row in rows {
            day_rows.push(row.expect("快照 agg_daily 行失败"));
        }
    }
    println!(
        "快照：范围外 agg_minute {} 行, agg_daily(app:%) {} 行",
        min_rows.len(),
        day_rows.len()
    );

    target
        .execute_batch("BEGIN IMMEDIATE;")
        .expect("开启事务失败");
    let rebuild_result = agg::rebuild_all(&target);
    if let Err(e) = rebuild_result {
        let _ = target.execute_batch("ROLLBACK;");
        panic!("rebuild_all 失败，已回滚: {e}");
    }
    // 范围外 agg_minute 插回（重算会重建全表 agg_minute，包括范围外——覆盖为原值）
    {
        let mut stmt = target
            .prepare("INSERT OR REPLACE INTO agg_minute (date, hour, minute, bucket_id, sum_value, count_value, max_event_rowid) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)")
            .expect("准备插回失败");
        for r in &min_rows {
            stmt.execute(rusqlite::params![
                r.date,
                r.hour,
                r.minute,
                r.bucket,
                r.sum,
                r.count,
                r.max_rowid
            ])
            .expect("插回 agg_minute 失败");
        }
    }
    // rebuild_all 会重建全部日期的 agg_daily(app:%)。范围外插回原值，
    // 范围内的删除后按 events 重算结果保留。
    {
        let mut stmt = target
            .prepare("INSERT OR REPLACE INTO agg_daily (date, bucket_id, sum_value, count_value) VALUES (?1, ?2, ?3, ?4)")
            .expect("准备插回 agg_daily 失败");
        for r in &day_rows {
            stmt.execute(rusqlite::params![r.date, r.bucket, r.sum, r.count])
                .expect("插回 agg_daily 失败");
        }
    }
    target.execute_batch("COMMIT;").expect("提交事务失败");
    println!("重算已提交。");

    // ---- 4. 复验：重新复制目标库 -> 重算 -> 逐分钟对比，应零差异 ----
    let copy2 = copy_db(&args.db, "verify");
    {
        let conn = Connection::open(&copy2).expect("打开复验副本失败");
        kynoptic_core::db::run_migrations(&conn).expect("复验副本迁移失败");
        agg::rebuild_all(&conn).expect("复验重算失败");
    }
    let vconn = Connection::open(&copy2).expect("重开复验副本失败");
    let after_current = read_minute_keys(
        &Connection::open(&args.db).expect("重开目标库失败"),
        &args.from,
        &args.to,
    );
    let after_expected = read_minute_keys(&vconn, &args.from, &args.to);
    print_compare(
        "apply 后复验（目标库当前值 vs events 重算期望值）",
        &args.from,
        &args.to,
        &after_current,
        &after_expected,
    );
    let remaining = diff_minutes(&after_current, &after_expected);
    if remaining.is_empty() {
        println!("PASS：范围内全部分钟与 events 重算期望一致。");
    } else {
        println!("FAIL：仍有 {} 个差异分钟。", remaining.len());
        exit(1);
    }
    println!(
        "临时副本保留于 {:?} 与 {:?} 备查。",
        copy1.parent(),
        copy2.parent()
    );
    let _ = Utc::now(); // 引用 chrono（事件时间均为 UTC RFC3339，由 core 处理）
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sqlite_header_magic_recognized() {
        assert!(is_sqlite_header(b"SQLite format 3\0"));
        assert!(is_sqlite_header(b"SQLite format 3\0plus trailing bytes"));
        assert!(!is_sqlite_header(b""));
        assert!(!is_sqlite_header(b"SQLite format "));
        // 常见误传：文本/日志/旧 Python pickle
        assert!(!is_sqlite_header(b"hello world"));
        assert!(!is_sqlite_header(b"PK\x03\x04rest-of-zip"));
    }
}
