//! SQLite 数据库层 —— 连接管理与 Repository 模式
//!
//! 本模块只负责连接生命周期（writer mutex + reader pool + 锁恢复），
//! 各表的 CRUD 按表拆分到子模块（events / sessions / metadata）。
//! 所有写操作通过 [`Database::with_writer`] 统一获取写连接，消除重复的锁获取样板。
//!
//! - WAL 模式 + 批量事务写入
//! - 读写连接分离：1 个写连接 + 8 个读连接池（Condvar 阻塞等待，耗尽降级临时连接）
//! - 自动保留策略（按天数清理旧事件）
//! - Schema 迁移系统（见 [`schema`])

use rusqlite::Connection;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard};
use std::time::Duration;

use crate::constants;

pub mod events;
pub mod metadata;
pub mod schema;
pub mod sessions;

// 子模块方法以 `impl Database` 形式分散定义，统一从根重新导出
pub use schema::{apply_pragmas, run_migrations, SCHEMA};

/// rusqlite 结果别名
pub type SqlResult<T> = rusqlite::Result<T>;

/// 解析数据库文件路径，主应用（lib.rs）与 ctl 共用同一逻辑，避免两者路径不一致。
///
/// 优先级：
/// 1. 环境变量 `KYNOPTIC_DB`（调试/测试覆盖）
/// 2. exe 同级 `data/kynoptic.db`（打包后标准位置——安装目录/data）
/// 3. cwd 候选（开发期：src-tauri/data 或 data，保护现有开发数据）
/// 4. 兜底 exe 同级
///
/// 此前主应用直接用相对路径 `"data/kynoptic.db"`，依赖 cwd：tauri dev（cwd=src-tauri）
/// 能工作，但打包后从开始菜单/托盘启动时 cwd 不可控（可能为 System32），数据会丢/分裂。
pub fn resolve_db_path() -> PathBuf {
    if let Ok(p) = std::env::var("KYNOPTIC_DB") {
        return PathBuf::from(p);
    }
    let filename = constants::DB_FILENAME;
    // 2) exe 同级 data/ —— 打包后的标准位置
    if let Ok(exe) = std::env::current_exe() {
        if let Some(exe_dir) = exe.parent() {
            let p = exe_dir.join("data").join(filename);
            if p.exists() {
                return p;
            }
        }
    }
    // 3) cwd 候选 —— 开发期保护现有数据（src-tauri/data 或 data）
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    for cand in [
        cwd.join("src-tauri/data").join(filename),
        cwd.join("data").join(filename),
    ] {
        if cand.exists() {
            return cand;
        }
    }
    // 3.5) 从 exe 往上找 src-tauri/data —— 开发期 exe 在 target/debug，
    //      数据在仓库根 src-tauri/data。从任意 cwd 启动都能定位到。
    if let Ok(exe) = std::env::current_exe() {
        if let Some(exe_dir) = exe.parent() {
            let mut dir = exe_dir.to_path_buf();
            for _ in 0..5 {
                let cand = dir.join("src-tauri/data").join(filename);
                if cand.exists() {
                    return cand;
                }
                match dir.parent() {
                    Some(p) => dir = p.to_path_buf(),
                    None => break,
                }
            }
        }
    }
    // 4) 兜底 exe 同级（首次运行时创建在此）
    if let Ok(exe) = std::env::current_exe() {
        if let Some(exe_dir) = exe.parent() {
            return exe_dir.join("data").join(filename);
        }
    }
    cwd.join("data").join(filename)
}

/// 确保主 DB 路径上有一份完整数据。
///
/// 开发期可能因 cwd 不同产生两份 DB（cargo run 写根 data/，tauri dev 写 src-tauri/data/）。
/// 本函数在目标路径不存在但某处存在旧库时，把旧库复制过来（取行数最多的那份），
/// 避免历史数据丢失。目标已存在则什么都不做。
pub fn ensure_primary_db(target: &Path) {
    use std::fs;
    if target.exists() {
        return;
    }
    // 收集所有候选位置中实际存在的 DB
    let filename = constants::DB_FILENAME;
    let mut candidates: Vec<PathBuf> = Vec::new();
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    candidates.push(cwd.join("src-tauri/data").join(filename));
    candidates.push(cwd.join("data").join(filename));
    // 开发期 exe 在 target/debug 或 target/release，数据在仓库根的 src-tauri/data。
    // 从 exe 目录往上逐级查找 src-tauri/data（最多 5 级），覆盖从任意 cwd 启动的场景。
    if let Ok(exe) = std::env::current_exe() {
        if let Some(exe_dir) = exe.parent() {
            candidates.push(exe_dir.join("data").join(filename));
            let mut dir = exe_dir.to_path_buf();
            for _ in 0..5 {
                let cand = dir.join("src-tauri/data").join(filename);
                candidates.push(cand.clone());
                match dir.parent() {
                    Some(p) => dir = p.to_path_buf(),
                    None => break,
                }
            }
        }
    }

    // 选行数最多的那份作为迁移源（最完整）
    let pick = candidates
        .into_iter()
        .filter(|p| p.exists())
        .map(|p| {
            // 用只读连接数 events 行数，失败跳过
            let rows = Connection::open(&p)
                .ok()
                .and_then(|c| {
                    c.query_row("SELECT COUNT(*) FROM events", [], |r| r.get::<_, i64>(0))
                        .ok()
                })
                .unwrap_or(0);
            (p, rows)
        })
        .max_by_key(|(_, n)| *n)
        .map(|(p, _)| p);

    if let Some(src) = pick {
        if let Some(parent) = target.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let _ = fs::copy(&src, target);
    }
}

// === 读连接池 ===

struct ReaderPool {
    conns: Mutex<Vec<Connection>>,
    cvar: Condvar,
}

impl ReaderPool {
    fn new(conns: Vec<Connection>) -> Self {
        Self {
            conns: Mutex::new(conns),
            cvar: Condvar::new(),
        }
    }

    /// 尝试从池中借出读连接，最多等待 `max_waits` 个周期（每周期 5s）。
    ///
    /// 返回 `Some(conn)` 表示借出成功（调用方负责归还）；
    /// 返回 `None` 表示等待超时——调用方应降级（如新建临时连接），避免无限阻塞。
    fn try_acquire(&self, max_waits: u32) -> Option<Connection> {
        let mut guard = self.conns.lock().unwrap_or_else(|e| {
            log::error!("读连接池 mutex 中毒: {e}");
            e.into_inner()
        });
        let mut waits = 0u32;
        while guard.is_empty() {
            if waits >= max_waits {
                log::error!("读连接池等待超过 {max_waits} 个周期仍无可用连接，触发降级");
                return None;
            }
            let (new_guard, timeout) = self
                .cvar
                .wait_timeout(guard, Duration::from_secs(5))
                .unwrap_or_else(|e| {
                    log::error!("读连接池 Condvar 等待失败: {e}");
                    e.into_inner()
                });
            guard = new_guard;
            if guard.is_empty() && timeout.timed_out() {
                waits += 1;
                log::warn!("读连接池等待超时 ({waits}/{max_waits})，所有连接都在使用中");
            }
        }
        guard.pop()
    }

    fn return_conn(&self, conn: Connection) {
        if let Ok(mut guard) = self.conns.lock() {
            guard.push(conn);
            drop(guard);
            self.cvar.notify_one();
        }
    }
}

/// 从连接池借出的读连接，Drop 时按策略处理：
/// - `Pooled`：归还到连接池（正常路径）
/// - `Temporary`：直接关闭（连接池耗尽时的降级兜底，用完即弃）
pub struct PooledConn<'a> {
    conn: Option<Connection>,
    pool: Option<&'a ReaderPool>,
}

impl<'a> Drop for PooledConn<'a> {
    fn drop(&mut self) {
        if let Some(conn) = self.conn.take() {
            match self.pool {
                Some(pool) => pool.return_conn(conn),
                None => { /* 临时连接：Drop 时直接关闭，不归还 */ }
            }
        }
    }
}

impl<'a> Deref for PooledConn<'a> {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        self.conn.as_ref().expect("connection already returned")
    }
}

// === Database ===

pub struct Database {
    db_path: String,
    writer: Mutex<Connection>,
    readers: ReaderPool,
    retention_days: i64,
    /// 读连接池降级次数（耗尽后改用临时/内存连接）。
    /// 单调递增，供运维观测；持续增长说明读负载超出池容量。
    reader_degraded: AtomicU64,
}

impl Database {
    pub fn open(path: &str) -> SqlResult<Self> {
        if let Some(parent) = Path::new(path).parent() {
            std::fs::create_dir_all(parent).ok();
        }

        let writer = Connection::open(path)?;
        writer.execute_batch(SCHEMA)?;
        run_migrations(&writer);

        let mut readers = Vec::with_capacity(constants::READER_POOL_SIZE);
        for _ in 0..constants::READER_POOL_SIZE {
            let r = Connection::open(path)?;
            apply_pragmas(&r)?;
            readers.push(r);
        }

        Ok(Self {
            db_path: path.to_string(),
            writer: Mutex::new(writer),
            readers: ReaderPool::new(readers),
            retention_days: constants::DEFAULT_RETENTION_DAYS,
            reader_degraded: AtomicU64::new(0),
        })
    }

    /// 借出读连接。
    ///
    /// 正常路径：从连接池借出，Drop 时归还。
    /// 降级路径：若连接池在 `READER_POOL_MAX_WAITS` 个周期（默认 3×5s=15s）后仍耗尽，
    /// 新建一个临时读连接返回（Drop 时直接关闭不归还），保证调用方永不无限阻塞。
    pub fn reader(&self) -> PooledConn<'_> {
        match self.readers.try_acquire(constants::READER_POOL_MAX_WAITS) {
            Some(conn) => PooledConn {
                conn: Some(conn),
                pool: Some(&self.readers),
            },
            None => {
                // 连接池耗尽兜底：新建临时连接（只读模式更安全）
                match Connection::open_with_flags(
                    &self.db_path,
                    rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
                        | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
                ) {
                    Ok(conn) => {
                        let _ = apply_pragmas(&conn);
                        self.reader_degraded.fetch_add(1, Ordering::Relaxed);
                        log::warn!(
                            "已降级为临时读连接（累计 {} 次）",
                            self.reader_degraded.load(Ordering::Relaxed)
                        );
                        PooledConn {
                            conn: Some(conn),
                            pool: None,
                        }
                    }
                    Err(e) => {
                        // 极端情况：连文件都打不开。返回内存库占位以避免 panic，
                        // 后续查询会拿到空结果（比死锁更可控）。
                        self.reader_degraded.fetch_add(1, Ordering::Relaxed);
                        log::error!(
                            "临时读连接创建失败，回退内存库（累计降级 {} 次）: {e}",
                            self.reader_degraded.load(Ordering::Relaxed)
                        );
                        let conn = Connection::open_in_memory().unwrap_or_else(|e| {
                            panic!("内存库也无法创建，数据库层彻底不可用: {e}")
                        });
                        PooledConn {
                            conn: Some(conn),
                            pool: None,
                        }
                    }
                }
            }
        }
    }

    /// 读连接池累计降级次数。
    ///
    /// 非零说明曾发生读池耗尽（改用临时/内存连接）。持续增长通常意味着：
    /// - 某个读连接被长期持有未归还（如长查询、死循环）
    /// - 读负载超出 `READER_POOL_SIZE`，需考虑扩容
    ///
    /// 该值单调递增，供监控/状态接口读取。
    pub fn reader_degraded_count(&self) -> u64 {
        self.reader_degraded.load(Ordering::Relaxed)
    }

    /// 统一的写连接获取入口（消除 14 处 `lock_writer` 样板）。
    ///
    /// 内部处理：Mutex 中毒恢复 + 连接重开 + 重试。若最终仍无法获取锁，
    /// 调用 `f` 不会被触发，闭包返回值由 `on_unavailable` 提供。
    ///
    /// 推荐用法（写方法返回 `()` 的场景）：
    /// ```ignore
    /// self.with_writer(|conn| {
    ///     let _ = conn.execute("UPDATE ...", params![...]);
    /// }, || log::warn!("写连接不可用，跳过"));
    /// ```
    pub fn with_writer<R>(
        &self,
        f: impl FnOnce(&MutexGuard<'_, Connection>) -> R,
        on_unavailable: impl FnOnce() -> R,
    ) -> R {
        match lock_writer(&self.writer, &self.db_path) {
            Some(guard) => f(&guard),
            None => on_unavailable(),
        }
    }

    /// 完整的维护操作：清理 + WAL 检查点 + VACUUM 压缩
    pub fn maintenance(&self) {
        self.cleanup_old_events();
        self.cleanup_old_sessions();
        self.refresh_daily_agg();
        let Some(conn) = lock_writer(&self.writer, &self.db_path) else {
            return;
        };
        log::info!("正在执行 WAL 检查点...");
        let _ = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
        log::info!("正在压缩数据库...");
        if let Err(e) = conn.execute_batch("VACUUM;") {
            log::error!("VACUUM 失败: {e}");
        } else {
            log::info!("数据库压缩完成");
        }
    }

    /// 刷新 daily_agg（异常检测的历史基线）——重算最近 2 天（今天 + 昨天）。
    ///
    /// 此前 daily_agg 只在 `ctl recompute` 手动刷新,采集器运行期间基线会滞后。
    /// 维护线程每天调用一次即可让 anomaly 的历史均值 APM 保持新鲜。
    /// 走写连接（daily_agg 是写操作），失败仅 log,不影响后续维护步骤。
    pub(crate) fn refresh_daily_agg(&self) {
        self.with_writer(
            |conn| match crate::daily_agg::recompute_recent_days(conn, 2) {
                Ok(n) if n > 0 => log::info!("daily_agg 已刷新（{} 天有变化）", n),
                Ok(_) => {}
                Err(e) => log::warn!("daily_agg 刷新失败: {e}"),
            },
            || log::warn!("daily_agg 刷新跳过：写连接不可用"),
        );
    }
}

/// 带超时的写锁获取。busy_timeout PRAGMA 已经在 apply_pragmas 里设过，
/// 但 std::sync::Mutex 本身可能因 panic 而中毒；本函数额外做：
///   1. 最多 5 次重试（应对短暂竞态）
///   2. 失败后尝试重开连接恢复
///   3. 全部失败返回 None，调用方跳过此操作（不 panic）
pub(crate) fn lock_writer<'a>(
    writer: &'a Mutex<Connection>,
    db_path: &str,
) -> Option<std::sync::MutexGuard<'a, Connection>> {
    const MAX_RETRIES: u32 = 5;
    const RETRY_BACKOFF_MS: u64 = 20;

    for attempt in 1..=MAX_RETRIES {
        match writer.lock() {
            Ok(guard) => return Some(guard),
            Err(poisoned) => {
                log::error!("写连接 mutex 中毒 (第 {attempt}/{MAX_RETRIES} 次)，尝试恢复...");
                // 取出中毒的连接并替换为新连接
                drop(poisoned.into_inner());
                match Connection::open(db_path) {
                    Ok(new_conn) => {
                        let _ = apply_pragmas(&new_conn);
                        let _ = new_conn.execute_batch(SCHEMA);
                        if let Ok(mut g) = writer.lock() {
                            *g = new_conn;
                            log::info!("写连接已恢复");
                            return Some(g);
                        }
                    }
                    Err(err) => {
                        log::error!("写连接恢复失败 (第 {attempt}/{MAX_RETRIES}): {err}");
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(
                    RETRY_BACKOFF_MS * attempt as u64,
                ));
            }
        }
    }
    log::error!("写锁获取失败，已达最大重试次数，操作跳过");
    None
}
