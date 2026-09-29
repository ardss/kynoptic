//! 持久化去重高水位（平台审查 item 11）。
//!
//! notification/calendar 的去重窗此前只是进程内内存（Cell）：采集器/托盘每次
//! 重启后 `last_event_time` / `emitted_keys` 清零，`Get-WinEvent` 日志窗口里
//! 仍存在的旧条目被整体重放落库，产生内容完全重复的行（retention=0 下重复行
//! 永久留存、统计双计）。
//!
//! 现在：把去重高水位落到 metadata 表（与 events 水位 / clipboard_salt 同一
//! 键值表）。每次采集取「metadata 持久基线 ∪ 本进程内存态」较新者做去重
//! 基线，并把推进后的高水位写回。
//!
//! 全程尽力而为：按进程开一条**轻量** `rusqlite` 连接（WAL + busy_timeout，
//! 与主池同口径，但不开第二套 reader/writer 池以免干扰主写者的 checkpoint）；
//! 库打不开 / 被锁 / metadata 表不存在 / 库文件不存在时静默降级为进程内去重
//! （旧行为），绝不阻塞采集。
//!
//! 库路径：优先用 [`set_db_path`] 登记的路径（`collector::start_collection_custom`
//! 在监控线程启动前登记，即采集器实际在写的库）——跨进程去重高水位必须与被
//! 去重的事件落在同一个库里。此前轻量连接固定走
//! [`crate::db::resolve_db_path`] 默认解析、无视 `--db` 覆盖：自定义部署会
//! 读到别人库的高水位并把自己的高水位 upsert 回默认库（双向污染，标准部署
//! 托盘重启后同样漏记），且 `rusqlite::Connection::open` 对不存在路径还会
//! 静默创建空库文件。未登记路径时退回 resolve_db_path，且库文件不存在时
//! 降级而不建库（与 CLI 的 db_missing 存在性门禁同口径）。

use std::sync::Mutex;
use std::time::Duration;

/// 每进程共享的轻量连接（notification/calendar 共用）。`Mutex` 串行化访问；
/// `Connection` 非 Sync，放 `Mutex` 后满足。连同打开时的路径一起记录——
/// 路径变更（同进程换库重启）后重新打开。
struct DedupConn {
    conn: rusqlite::Connection,
    path: std::path::PathBuf,
}
static DEDUP_CONN: Mutex<Option<DedupConn>> = Mutex::new(None);

/// 采集器登记的 db 路径（单一来源）：轻量连接优先打开采集器实际在写的库。
static DEDUP_DB_PATH: Mutex<Option<std::path::PathBuf>> = Mutex::new(None);

/// 登记入口：`collector::start_collection_custom` 在监控线程启动前调用。
/// 同库重复登记幂等；路径变更时丢弃已开的旧库连接，下次访问按新路径重开
/// （锁序与 [`conn`] 一致：先取路径锁并释放、再取连接锁，无环）。
pub(crate) fn set_db_path(path: &std::path::Path) {
    *DEDUP_DB_PATH.lock().unwrap() = Some(path.to_path_buf());
    let mut c = DEDUP_CONN.lock().unwrap();
    if let Some(e) = c.as_ref() {
        if e.path != path {
            *c = None;
        }
    }
}

/// 取（并惰性打开）轻量连接，返回持锁 guard（guard 存活期间连接可用）。
/// 打开失败 / 库文件不存在返回 None（调用方据此降级为进程内去重）。
fn conn() -> Option<std::sync::MutexGuard<'static, Option<DedupConn>>> {
    // 先读登记路径并立即释放路径锁（避免与 set_db_path 的锁序成环）；
    // 未登记时退回默认解析
    let path = {
        let reg = DEDUP_DB_PATH.lock().unwrap();
        reg.clone().unwrap_or_else(crate::db::resolve_db_path)
    };
    let mut g = DEDUP_CONN.lock().unwrap();
    if let Some(e) = g.as_ref() {
        if e.path == path {
            return Some(g);
        }
        *g = None; // 换库：重开
    }
    // 空库门禁：库文件不存在则降级而不创建——Connection::open 会对不存在
    // 路径生成一个 0 功能空 kynoptic.db（默认解析路径在 exe 旁 data/ 下）；
    // 库文件不存在时 metadata 表必然也不存在，降级即旧行为（不回归）。
    if !path.exists() {
        log::debug!("dedup 轻量连接: 库文件不存在 {path:?}，降级为进程内去重");
        return None;
    }
    match rusqlite::Connection::open(&path) {
        Ok(c) => {
            // 容忍主池写者短暂持锁；WAL 允许多连接
            let _ = c.busy_timeout(Duration::from_secs(5));
            *g = Some(DedupConn { conn: c, path });
            Some(g)
        }
        Err(e) => {
            log::debug!("dedup 轻量连接打开失败: {e}（降级为进程内去重）");
            None
        }
    }
}

/// 读持久化去重高水位。键不存在 / 读失败 → None。
pub(crate) fn load_baseline(key: &str) -> Option<String> {
    let g = conn()?;
    use rusqlite::params;
    g.as_ref().and_then(|e| {
        e.conn
            .query_row(
                "SELECT value FROM metadata WHERE key = ?1",
                params![key],
                |r| r.get::<_, String>(0),
            )
            .ok()
    })
}

/// 写去重高水位（upsert）。尽力而为：失败只留日志，不阻塞采集。
pub(crate) fn save_baseline(key: &str, value: &str) {
    let Some(g) = conn() else {
        return;
    };
    use rusqlite::params;
    if let Some(e) = g.as_ref() {
        let res = e.conn.execute(
            "INSERT INTO metadata (key, value) VALUES (?1, ?2) \
             ON CONFLICT(key) DO UPDATE SET value = ?2",
            params![key, value],
        );
        if let Err(e) = res {
            log::debug!("dedup 高水位写入失败: {e}");
        }
    }
}
