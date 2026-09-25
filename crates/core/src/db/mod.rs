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

use rusqlite::{params, Connection};
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard};
use std::thread;
use std::time::Duration;

use crate::constants;

pub mod agg;
pub mod events;
pub mod metadata;
pub mod schema;
pub mod sessions;

// 子模块方法以 `impl Database` 形式分散定义，统一从根重新导出
pub use schema::{apply_pragmas, apply_pragmas_readonly, run_migrations, SCHEMA};

/// rusqlite 结果别名
pub type SqlResult<T> = rusqlite::Result<T>;

// === 库降级全局标记（库损坏静默零值修复 2026-09） ===
//
// 此前库文件损坏（截半/半截写入）时查询静默回退零值，/api/status 无任何
// 降级标志——用户看到"今天什么都没干"而非"库坏了"。这里用进程级标记承接
// open 时 quick_check 与 WAL 水位对账的结论，供 dash /api/status 以
// [`db_degraded_reason`] 暴露为 db_degraded 字段（同进程内生效：tray 进程
// 同时持有 Database 与面板服务）。
static DB_DEGRADED: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// 标记数据库处于降级状态（损坏/数据回退）。只记录首个原因；留档告警与
/// 0 字节/云同步检测同级（archive_write_failure），不阻塞启动。
pub fn mark_db_degraded(reason: &str) {
    log::error!("数据库降级: {reason}");
    crate::collector::archive_write_failure(&format!("数据库降级: {reason}"));
    if let Ok(mut guard) = DB_DEGRADED.lock() {
        if guard.is_none() {
            *guard = Some(reason.to_string());
        }
    }
}

/// 当前降级原因（None = 正常）。供 dash /api/status 暴露 db_degraded 字段。
pub fn db_degraded_reason() -> Option<String> {
    DB_DEGRADED.lock().ok().and_then(|g| g.clone())
}

/// 降级旗标文件名（写在 db 同目录，托盘每 2s 轮询，与 dashboard-port.txt
/// 同模式——降级此前只有面板横幅一个用户可见面，托盘图标保持绿色）。
const DEGRADED_FLAG: &str = "degraded.flag";

/// [`mark_db_degraded`] 的带路径版：额外在 db 目录写 degraded.flag，
/// 让托盘（跨进程）也能看到降级状态。旗标写入尽力而为，失败不影响留档。
pub fn mark_db_degraded_at(db_file: &Path, reason: &str) {
    mark_db_degraded(reason);
    if let Some(dir) = db_file.parent() {
        let _ = std::fs::write(dir.join(DEGRADED_FLAG), format!("{reason}\n"));
    }
}

/// 健康库打开时清除遗留降级旗标（上次运行降级、本次正常 → 旗标不得残留，
/// 否则托盘永远黄三角）。尽力而为。
pub fn clear_degraded_flag(db_file: &Path) {
    if let Some(dir) = db_file.parent() {
        let _ = std::fs::remove_file(dir.join(DEGRADED_FLAG));
    }
}

/// metadata 表里持久化的 events 水位键名（最近一次正常关闭时的 max(rowid)）。
const EVENTS_WATERMARK_KEY: &str = "events_watermark_max_rowid";

/// WAL 水位对账（WAL 静默蒸发防护 2026-09）：连接存活期间 WAL 被外部截断/
/// 删除（云同步冲突、手动清理）时，SQLite 恢复把截断后的尾部当无效帧丢弃——
/// quick_check=ok、正常启动、已提交事务静默蒸发且零告警。对策：正常停机
/// （[`Database::mark_stopping`]）把 events max(rowid) 持久化到 metadata 作
/// 水位；open 时若当前 max(rowid) **回退**到水位之下，说明持久化数据蒸发，
/// 走 [`mark_db_degraded`] 留档并暴露。同时把水位刷新为当前值（对账一次性）。
/// 返回回退告警文本（无回退返回 None）。
fn reconcile_events_watermark(conn: &Connection) -> Option<String> {
    let current: i64 = conn
        .query_row("SELECT COALESCE(MAX(rowid), 0) FROM events", [], |r| {
            r.get(0)
        })
        .unwrap_or(0);
    let persisted: i64 = conn
        .query_row(
            "SELECT value FROM metadata WHERE key = ?1",
            params![EVENTS_WATERMARK_KEY],
            |r| r.get::<_, String>(0),
        )
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let _ = conn.execute(
        "INSERT INTO metadata (key, value) VALUES (?1, ?2) \
         ON CONFLICT(key) DO UPDATE SET value = ?2",
        params![EVENTS_WATERMARK_KEY, current.to_string()],
    );
    if persisted > current {
        return Some(format!(
            "events 最大行号从 {persisted} 回退到 {current}：已提交数据丢失（WAL 可能被外部截断/删除，如云同步冲突或手动清理），请检查备份"
        ));
    }
    None
}

/// 长路径阈值（实测 2026-09-25：>260 字符的非 verbatim 路径 rusqlite 报
/// "unable to open database file"，而 std 文件 API 同深度可写成功——SQLite
/// 走非 verbatim 的 CreateFileW 受 MAX_PATH 限制）。低于 260 留余量触发，
/// 避免对正常路径做无谓的 verbatim 改写（verbatim 前缀会禁用 `/` 与 `..`
/// 的常规解析，短路径保持原样行为零变化）。
const SQLITE_LONG_PATH_THRESHOLD: usize = 240;

/// 把用户提供的库路径规范化为 SQLite 可打开的形式（收口点，[`Database::open`]
/// 与 CLI 共用）：绝对路径且长度超阈值、又尚未带 verbatim 前缀时，加
/// `\\?\`（UNC 路径加 `\\?\UNC\`）前缀绕过 MAX_PATH。相对路径与短路径原样返回。
pub fn normalize_sqlite_path(path: &str) -> String {
    if path.len() < SQLITE_LONG_PATH_THRESHOLD {
        return path.to_string();
    }
    if path.starts_with(r"\\?\") {
        return path.to_string();
    }
    let absolute = std::path::absolute(path).unwrap_or_else(|_| PathBuf::from(path));
    let s = absolute.to_string_lossy();
    if s.starts_with(r"\\?\") {
        return s.into_owned();
    }
    if let Some(unc) = s.strip_prefix(r"\\") {
        return format!(r"\\?\UNC\{unc}");
    }
    format!(r"\\?\{s}")
}

/// 超长路径的人话提示（打开失败时追加在错误信息里，帮用户定位根因，
/// 而不是只看到 SQLite 的 "unable to open database file"）。阈值 240 字节
/// 是提前量（留余量做 verbatim 改写），此时路径未必真超 260，文案只说
/// "接近"不说 "超过"；长度按字符计（非 UTF-8 字节数），与 MAX_PATH 的
/// 单位一致。
pub fn long_path_hint(path: &str) -> Option<String> {
    (path.len() >= SQLITE_LONG_PATH_THRESHOLD).then(|| {
        format!(
            "路径长度 {} 个字符，接近 Windows 260 字符路径上限，可能是打不开的原因之一；请把数据库放到更浅的目录，或用较短的 --db 路径",
            path.chars().count()
        )
    })
}

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

/// 确保主 DB 路径可读性诊断：对目标路径开只读连接跑 `PRAGMA quick_check`，
/// 把结果转成人类可读的诊断文本（完整 / 损坏 / 打不开）。供打开失败时的
/// 错误信息组装使用；本函数只读不写、不做任何复制或合并。
pub fn diagnose_open_failure(path: &std::path::Path) -> String {
    let raw = path.to_string_lossy();
    match Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY) {
        Ok(conn) => match conn.query_row("PRAGMA quick_check", [], |r| r.get::<_, String>(0)) {
            Ok(msg) if msg == "ok" => "数据库可读且完整；可能为权限/磁盘/文件占用问题".to_string(),
            Ok(msg) => format!(
                "数据库完整性检查失败: {msg}（建议先用副本尝试 kynoptic-aggrepair 或恢复备份）"
            ),
            Err(e) => format!("quick_check 执行失败: {e}"),
        },
        Err(e) => {
            // 只读也打不开时补长路径根因提示（帮助区分"路径过长"与权限问题）
            match long_path_hint(&raw) {
                Some(hint) => format!("数据库无法打开(只读): {e}；{hint}"),
                None => format!("数据库无法打开(只读): {e}"),
            }
        }
    }
}

/// 判定路径是否处于云同步目录（OneDrive KFM 等）。两项启发式：
/// 1. 祖先链上有名为 "OneDrive" 的路径组件（大小写不敏感）；
/// 2. 文件带 Windows CLOUD_FILE_ATTRIBUTE（0x00400000，按需下载/云占位标记，
///    真机探针验证普通本地文件不带该位）。
fn is_cloud_sync_path(p: &Path) -> bool {
    if p.ancestors().any(|a| {
        a.file_name()
            .is_some_and(|n| n.to_string_lossy().eq_ignore_ascii_case("OneDrive"))
    }) {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const CLOUD_FILE_ATTRIBUTE: u32 = 0x0040_0000;
        std::fs::metadata(p)
            .map(|m| m.file_attributes() & CLOUD_FILE_ATTRIBUTE != 0)
            .unwrap_or(false)
    }
    #[cfg(not(windows))]
    {
        false
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
        // 审查 P2：mutex 中毒（持有方 panic 过）时不能静默丢弃连接（池悄悄
        // 缩小）。与 try_acquire 的 Condvar 等待路径一致，取中毒后的内部数据
        // 继续归还。
        let mut guard = self.conns.lock().unwrap_or_else(|e| e.into_inner());
        guard.push(conn);
        drop(guard);
        self.cvar.notify_one();
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
    /// 后台聚合回填完成信号（done, Condvar）。open 时无回填任务则立即置位。
    backfill_done: std::sync::Arc<(Mutex<bool>, std::sync::Condvar)>,
    /// 当前采集会话 id（start_session 写入，0 = 尚未开始）。
    /// 审查 P0：Event.session_id 此前永远是 None（types.rs Event::new 硬编码
    /// None 且无人回填），全库 2.3 万事件 session_id 全为 NULL——ghost 清扫
    /// 的"取该 session 最后事件时间/事件数"全部退化为 start_time/0，sessions
    /// 表事实失效。writer 在落库前从此原子量补盖 session_id。
    current_session: std::sync::atomic::AtomicI64,
    /// 停机旗标（审查 MEDIUM）：采集器 shutdown 置位后，maintenance() 跳过
    /// checkpoint/VACUUM 等持写互斥体的重活——否则 shutdown 对 Maintenance
    /// 线程的 join 会被大库 VACUUM 阻塞数分钟（心跳停滞被 watchdog 误杀）。
    stopping: std::sync::atomic::AtomicBool,
}

/// RAII：回填线程退出时（无论成功/失败/panic）置完成信号。
struct BackfillDoneGuard<'a>(&'a std::sync::Arc<(Mutex<bool>, std::sync::Condvar)>);

impl Drop for BackfillDoneGuard<'_> {
    fn drop(&mut self) {
        if let Ok(mut d) = self.0 .0.lock() {
            *d = true;
        }
        self.0 .1.notify_all();
    }
}

impl Database {
    pub fn open(path: &str) -> SqlResult<Self> {
        // 长路径收口（审查发现 2026-09-25）：>260 字符路径 SQLite 打不开而
        // std 文件 API 可以——统一在此规范化为 verbatim 形式，所有调用方
        // （tray/dash/mcp/CLI 的 Database::open）一次修复。
        let path_owned = normalize_sqlite_path(path);
        let path: &str = &path_owned;
        if let Some(parent) = Path::new(path).parent() {
            std::fs::create_dir_all(parent).ok();
        }

        // 空库文件检测（实测：SQLite 把 0 字节文件当合法新库，SCHEMA 静默建
        // 全表后满血运行，历史数据凭空消失且零告警——diagnose_open_failure
        // 只在打开失败时才被调用）。K 盘闪断/同步冲突清理/半截写入都会留下
        // 0 字节残留。仅留档告警，不阻塞启动（无法区分"被清空"与"首次建库"）。
        let db_file = Path::new(path);
        if db_file.is_file()
            && std::fs::metadata(db_file)
                .map(|m| m.len() == 0)
                .unwrap_or(false)
        {
            let msg = "检测到空数据库文件（0 字节）：此前的数据可能被外部清空、同步冲突或半截写入覆盖，已按全新库启动";
            log::warn!("{msg}");
            crate::collector::archive_write_failure(msg);
        }
        // 云同步目录检测：db+wal 双文件被 OneDrive 等独立上传，冲突恢复时
        // 版本错配即静默丢数据或库损坏（实测 db/wal 错配多为 quick_check=ok
        // 的静默空库）。仅告警不阻断，面板侧横幅属 dash 域另行接线。
        if is_cloud_sync_path(db_file) {
            let msg = "数据库位于云同步目录（OneDrive 等）下：db 与 -wal 双文件被独立上传，恢复/冲突时可能静默丢失数据，建议将数据目录迁移出同步路径";
            log::warn!("{msg}");
            crate::collector::archive_write_failure(msg);
        }

        let writer = match Connection::open(path) {
            Ok(c) => c,
            Err(e) => {
                // 打不开时补人话根因提示（超长路径是实测的可打开性陷阱）
                if let Some(hint) = long_path_hint(path) {
                    log::error!("数据库打开失败: {e}；{hint}");
                }
                return Err(e);
            }
        };
        // 审查 P1：writer 也必须带 busy_timeout/WAL——后台回填线程持有独立写
        // 连接（BEGIN IMMEDIATE），热重载重启后新 writer 若 busy_timeout=0 会
        // 立即 SQLITE_BUSY 降级丢批。
        let _ = crate::db::schema::apply_pragmas(&writer);
        // 打开时 quick_check 廉价门槛（对照 0 字节检测 2026-09）：主库截半等
        // 损坏下 SCHEMA/查询仍可能"成功"，随后所有端点静默回退零值。open 时
        // 跑一次 quick_check，非 ok 即走 mark_db_degraded 留档并暴露。
        match writer.query_row("PRAGMA quick_check", [], |r| r.get::<_, String>(0)) {
            Ok(s) if s != "ok" => mark_db_degraded_at(
                std::path::Path::new(path),
                &format!(
                    "数据库完整性检查失败: {s}（建议先用副本尝试 kynoptic-aggrepair 或恢复备份）"
                ),
            ),
            Err(e) => mark_db_degraded_at(
                std::path::Path::new(path),
                &format!("数据库完整性检查执行失败: {e}"),
            ),
            Ok(_) => {}
        }
        writer.execute_batch(SCHEMA)?;
        // 迁移失败硬失败（审查 P1）：宁可不启动，不带病运行
        run_migrations(&writer)?;
        // WAL 水位对账（见 reconcile_events_watermark 文档）
        if let Some(msg) = reconcile_events_watermark(&writer) {
            mark_db_degraded_at(std::path::Path::new(path), &msg);
        } else if db_degraded_reason().is_none() {
            // 本次 open 各项检查全部健康：清除上次运行可能遗留的降级旗标
            clear_degraded_flag(std::path::Path::new(path));
        }
        // 懒回填聚合读缓存（存量库首开一次）——**后台分块执行，不阻塞 open**。
        // perf3 2026-09 P0 实测：1M 事件存量库首开时同步回填把 Database::open
        // 阻塞 10.5 分钟（631,594 ms；目标 <500ms）。改为：open 只做廉价门槛
        // 检查（两条 EXISTS），分块（本地 date,hour）回填在后台线程执行——
        // 每块一个短事务（DELETE 该块聚合行 + events 重算），writer 的批量写入
        // （events 落库 + 同事务聚合增量）可在块间穿插；进度记在 metadata.agg_backfill_cursor，
        // 中断后下次 open 自动续跑。回填完成前聚合查询回退 events 现算
        // （正确但慢），原始 events 只读不动。
        let backfill_done: std::sync::Arc<(Mutex<bool>, std::sync::Condvar)> =
            std::sync::Arc::new((Mutex::new(false), std::sync::Condvar::new()));

        if agg::backfill_needed(&writer) {
            log::info!("检测到 agg 缓存缺失，转入后台分块回填（不阻塞启动）");
            let bg_path = path.to_string();
            let done_flag = backfill_done.clone();
            // 审查 LOW：spawn 失败被 let _ 静默吞掉——BackfillDoneGuard 在闭包内
            // 构造，spawn 失败则永远不置位（wait_for_backfill 吃满超时）。Err 分支
            // 显式记日志并手动置位完成信号。
            if let Err(e) = thread::Builder::new()
                .name("agg-backfill".into())
                .spawn(move || {
                    let _guard = BackfillDoneGuard(&done_flag);
                    match Connection::open(&bg_path) {
                        Ok(c) => {
                            if apply_pragmas(&c).is_err() {
                                log::warn!("agg 回填连接 PRAGMA 设置失败（继续，用默认配置）");
                            }
                            match agg::backfill_all(&c) {
                                Ok(n) => log::info!("agg 后台分块回填完成（{} 行 agg_minute）", n),
                                Err(e) => log::warn!("agg 后台分块回填失败（下次打开续跑）: {e}"),
                            }
                        }
                        Err(e) => log::warn!("agg 后台回填连接创建失败: {e}"),
                    }
                })
            {
                log::error!("agg 后台回填线程 spawn 失败，完成信号直接置位: {e}");
                if let Ok(mut d) = backfill_done.0.lock() {
                    *d = true;
                }
                backfill_done.1.notify_all();
            }
        } else if agg::read_cursor_incomplete(&writer) {
            // 上次分块回填中断（agg 已有部分行）：补齐剩余块
            let bg_path = path.to_string();
            let done_flag = backfill_done.clone();
            // 审查 LOW：同上——spawn 失败必须记日志并置位完成信号
            if let Err(e) = thread::Builder::new()
                .name("agg-backfill".into())
                .spawn(move || {
                    let _guard = BackfillDoneGuard(&done_flag);
                    match Connection::open(&bg_path) {
                        Ok(c) => {
                            let _ = apply_pragmas(&c);
                            match agg::backfill_all(&c) {
                                Ok(n) => log::info!("agg 后台分块回填续跑完成（{} 行）", n),
                                Err(e) => log::warn!("agg 后台分块回填补齐失败: {e}"),
                            }
                        }
                        Err(e) => log::warn!("agg 后台回填连接创建失败: {e}"),
                    }
                })
            {
                log::error!("agg 后台回填补齐线程 spawn 失败，完成信号直接置位: {e}");
                if let Ok(mut d) = backfill_done.0.lock() {
                    *d = true;
                }
                backfill_done.1.notify_all();
            }
        } else {
            // 欠聚合核对（P1，廉价：timestamp 下界 7 天 + 索引区间，见
            // agg::under_agg_dates 的取舍说明）：events 里应有聚合贡献的
            // 原始行（press/click/switch）最大 id 超过该日 agg_minute 已记录的
            // max_event_rowid → 该日欠聚合（历史 kill 中断/存量遗留），后台
            // 逐小时重算自愈。增量路径已与 events 落库同事务（见
            // insert_events_with_agg），此核对只兜底存量与极端故障。
            let lag_dates = agg::under_agg_dates(&writer);
            if lag_dates.is_empty() {
                // 无回填任务：立即置完成信号
                if let Ok(mut d) = backfill_done.0.lock() {
                    *d = true;
                }
                backfill_done.1.notify_all();
            } else {
                log::info!(
                    "检测到 {} 个本地日期欠聚合（events 有而 agg 无），后台自愈",
                    lag_dates.len()
                );
                let bg_path = path.to_string();
                let done_flag = backfill_done.clone();
                // 审查 LOW：同上——spawn 失败必须记日志并置位完成信号
                if let Err(e) =
                    thread::Builder::new()
                        .name("agg-lag-heal".into())
                        .spawn(move || {
                            let _guard = BackfillDoneGuard(&done_flag);
                            match Connection::open(&bg_path) {
                                Ok(c) => {
                                    let _ = apply_pragmas(&c);
                                    match agg::heal_under_agg(&c) {
                                        Ok(n) => log::info!("欠聚合自愈完成（{} 个日期）", n),
                                        Err(e) => log::warn!("欠聚合自愈失败: {e}"),
                                    }
                                }
                                Err(e) => log::warn!("欠聚合自愈连接创建失败: {e}"),
                            }
                        })
                {
                    log::error!("欠聚合自愈线程 spawn 失败，完成信号直接置位: {e}");
                    if let Ok(mut d) = backfill_done.0.lock() {
                        *d = true;
                    }
                    backfill_done.1.notify_all();
                }
            }
        }

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
            backfill_done,
            current_session: std::sync::atomic::AtomicI64::new(0),
            stopping: std::sync::atomic::AtomicBool::new(false),
        })
    }

    /// 标记停机：maintenance() 此后跳过 checkpoint/VACUUM 等重活
    /// （采集器 shutdown/Drop 调用，见字段文档）。
    pub fn mark_stopping(&self) {
        self.stopping
            .store(true, std::sync::atomic::Ordering::Release);
        // 正常停机时持久化 events 水位（供下次 open 的 WAL 回退对账，见
        // reconcile_events_watermark）。失败仅留日志，不阻塞关停。
        self.with_writer(
            |conn| {
                let n: i64 = conn
                    .query_row("SELECT COALESCE(MAX(rowid), 0) FROM events", [], |r| {
                        r.get(0)
                    })
                    .unwrap_or(0);
                if let Err(e) = conn.execute(
                    "INSERT INTO metadata (key, value) VALUES (?1, ?2) \
                     ON CONFLICT(key) DO UPDATE SET value = ?2",
                    params![EVENTS_WATERMARK_KEY, n.to_string()],
                ) {
                    log::warn!("停机水位持久化失败: {e}");
                }
            },
            || log::warn!("停机水位持久化跳过：写连接不可用"),
        );
    }

    fn is_stopping(&self) -> bool {
        self.stopping.load(std::sync::atomic::Ordering::Acquire)
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
                        // 审查 P2：旧实现 open_in_memory 失败即 panic。内存库
                        // 只在进程级资源耗尽（OOM/句柄耗尽）时失败，panic 会把
                        // 整个应用（含托盘与采集）拖垮，比"读暂时不可用"严重
                        // 得多。改为带退避的有限重试，仍失败则最后尝试读写打开
                        // 目标库文件（只读打开失败的常见原因是文件被删，读写
                        // 打开可重建）；全部失败时阻塞重试而非 panic——查询
                        // 侧暂时卡住可在资源恢复后自愈，进程崩溃不可逆。
                        let conn = loop {
                            match Connection::open_in_memory() {
                                Ok(c) => break c,
                                Err(e) => {
                                    self.reader_degraded.fetch_add(1, Ordering::Relaxed);
                                    log::error!(
                                        "内存库创建失败（累计降级 {} 次），500ms 后重试: {e}",
                                        self.reader_degraded.load(Ordering::Relaxed)
                                    );
                                    std::thread::sleep(std::time::Duration::from_millis(500));
                                    if let Ok(c) = Connection::open(&self.db_path) {
                                        log::warn!("已兜底为读写打开目标库文件");
                                        break c;
                                    }
                                }
                            }
                        };
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

    /// WAL 文件大小（字节）；文件不存在返回 None。
    pub fn wal_size_bytes(&self) -> Option<u64> {
        let p = format!("{}-wal", self.db_path);
        std::fs::metadata(&p).ok().map(|m| m.len())
    }

    /// 完整的维护操作：清理 + 欠聚合核对自愈 + WAL 检查点 + VACUUM 压缩
    pub fn maintenance(&self) {
        // 审查 MEDIUM：停机旗标置位即整体跳过——checkpoint/VACUUM 持写互斥体
        // 数分钟会让 shutdown 的 maintenance join 挂死（与"join 有界"注释矛盾，
        // 且心跳停滞 1800s 后被 watchdog 误杀）。维护是周期性的，跳过一次
        // 无损，下一周期（或下次会话）照常执行。
        if self.is_stopping() {
            log::info!("停机中：跳过本次数据库维护（不阻塞关停）");
            return;
        }
        self.cleanup_old_events();
        self.cleanup_old_sessions();
        self.refresh_daily_agg();
        // 每日欠聚合核对（P1 自愈）：不一致日期逐小时重算（无欠聚合时只是
        // 两条聚合查询的廉价核对）
        let healed = self.heal_agg_lag();
        if healed > 0 {
            log::info!("维护：欠聚合自愈完成（{} 个日期）", healed);
        }
        let Some(conn) = lock_writer(&self.writer, &self.db_path) else {
            return;
        };
        // 拿到写锁后再查一次停机旗标：上面的前置检查与拿锁之间维护可能已被
        // shutdown 追上——重活（checkpoint/VACUUM）必须在关停路径上让位。
        if self.is_stopping() {
            log::info!("停机中：跳过 WAL 检查点与 VACUUM（不阻塞关停）");
            return;
        }
        log::info!("正在执行 WAL 检查点...");
        wal_checkpoint_truncate(&conn);
        // Wave19 性能审查：retention=0 下库是 append-only，空闲页极少，
        // 每日 VACUUM 收益≈0 却要重写整库（1 年 4.5GB = 每天 ~13GB 白烧
        // IO + 等量瞬时磁盘峰值）。空闲页占比 < 10% 直接跳过。
        let freelist: i64 = conn
            .query_row("PRAGMA freelist_count", [], |r| r.get(0))
            .unwrap_or(0);
        let page_count: i64 = conn
            .query_row("PRAGMA page_count", [], |r| r.get(0))
            .unwrap_or(1);
        if page_count > 0 && (freelist as f64 / page_count as f64) >= 0.10 {
            log::info!("正在压缩数据库（空闲页 {freelist}/{page_count}）...");
            if let Err(e) = conn.execute_batch("VACUUM;") {
                log::error!("VACUUM 失败: {e}");
            } else {
                log::info!("数据库压缩完成");
            }
        } else {
            log::debug!("VACUUM 跳过：空闲页占比不足 10%（append-only 正常态）");
        }
        // VACUUM 全程经 WAL 重写（把 WAL 再次撑大），故检查点必须放在 VACUUM 之后，
        // 否则"维护后库大小"被未截断的 WAL 虚增近一倍（perf-write 2026-09 实测）。
        wal_checkpoint_truncate(&conn);
    }

    /// 等待后台聚合回填完成（最多 `timeout`）。供测试与需要在回填结束后
    /// 读取聚合缓存的调用方使用；超时返回 false。
    pub fn wait_for_backfill(&self, timeout: std::time::Duration) -> bool {
        let (lock, cvar) = &*self.backfill_done;
        let guard = match lock.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        if *guard {
            return true;
        }
        match cvar.wait_timeout_while(guard, timeout, |d| !*d) {
            Ok((g, _)) => *g,
            Err(_) => false,
        }
    }

    /// 刷新 daily_agg（异常检测的历史基线）——重算最近 2 天（今天 + 昨天）。
    ///
    /// 此前 daily_agg 只在 `ctl recompute` 手动刷新,采集器运行期间基线会滞后。
    /// 维护线程每天调用一次即可让 anomaly 的历史均值 APM 保持新鲜。
    /// 失败仅 log,不影响后续维护步骤。
    ///
    /// perf 审查（LOW）修复：单日重扫描成本随当日行数线性（合成基准 50 万行
    /// 达 13-18s），旧实现在写连接 Mutex 内完成全部扫描——占锁期间写批次停摆，
    /// 且不检查停机旗标、启动首刷还在 tray 启动路径上。现拆为
    /// [`crate::daily_agg::compute_day`]（读连接，锁外）+
    /// [`crate::daily_agg::upsert_day`]（写连接，锁内仅 UPSERT），并在停机
    /// 旗标置位时整体跳过（与 maintenance 一致，不阻塞关停）。
    pub(crate) fn refresh_daily_agg(&self) {
        if self.is_stopping() {
            log::info!("停机中：跳过 daily_agg 刷新");
            return;
        }
        // 读侧计算：borrow 读连接池，不碰写锁
        let today = chrono::Local::now();
        let dates: Vec<String> = (0..2)
            .map(|i| {
                (today - chrono::Duration::days(i))
                    .format("%Y-%m-%d")
                    .to_string()
            })
            .collect();
        let mut computed: Vec<(String, crate::daily_agg::DayStats)> = Vec::new();
        {
            let reader = self.reader();
            for date in &dates {
                match crate::daily_agg::compute_day(&reader, date) {
                    Ok(s) => computed.push((date.clone(), s)),
                    Err(e) => log::warn!("daily_agg 读侧计算失败（{date}）: {e}"),
                }
            }
        }
        if computed.is_empty() {
            return;
        }
        // 写侧落库：锁内只做两条廉价 UPSERT
        self.with_writer(
            |conn| {
                for (date, s) in &computed {
                    if let Err(e) = crate::daily_agg::upsert_day(conn, date, s) {
                        log::warn!("daily_agg 刷新失败（{date}）: {e}");
                    }
                }
            },
            || log::warn!("daily_agg 刷新跳过：写连接不可用"),
        );
    }
}

/// TRUNCATE 检查点（审查 33-F8）：存在活跃 reader（dashboard 常驻只读池 /
/// core 读池持未结束读事务）时 busy=1，WAL 无法截断——旧实现 execute_batch
/// 丢弃返回值，「检查点完成」与实际不符。这里读出 busy 状态留痕日志。
fn wal_checkpoint_truncate(conn: &Connection) {
    match conn.query_row("PRAGMA wal_checkpoint(TRUNCATE);", [], |r| {
        r.get::<_, i64>(0)
    }) {
        Ok(0) => {}
        Ok(busy) => log::info!(
            "WAL 检查点未能截断（busy={busy}，存在活跃 reader）；journal_size_limit 会在其后下一笔写入把 WAL 回落到 16MB 封顶"
        ),
        Err(e) => log::debug!("WAL 检查点查询失败（跳过）: {e}"),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 短路径与已 verbatim 的路径必须原样返回（行为零变化）
    #[test]
    fn normalize_sqlite_path_passthrough() {
        assert_eq!(
            normalize_sqlite_path("data\\kynoptic.db"),
            "data\\kynoptic.db"
        );
        let short = r"C:\tmp\kynoptic.db";
        assert_eq!(normalize_sqlite_path(short), short);
        let verbatim = format!(r"\\?\C:\{}", "x".repeat(300));
        assert_eq!(normalize_sqlite_path(&verbatim), verbatim);
    }

    /// 超长绝对路径加 \\?\ 前缀；UNC 超长路径加 \\?\UNC\ 前缀
    #[test]
    fn normalize_sqlite_path_prefixes_long_paths() {
        let long = format!(r"C:\{}", "a\\".repeat(130)); // > 240 字符
        assert!(long.len() >= SQLITE_LONG_PATH_THRESHOLD);
        let got = normalize_sqlite_path(&long);
        assert!(got.starts_with(r"\\?\C:\"), "got {got:?}");
        // 前缀只是加在最前，路径本体不变
        assert_eq!(&got[4..], long);
        let unc = format!(r"\\server\share\{}", "a\\".repeat(130));
        let got_unc = normalize_sqlite_path(&unc);
        assert!(
            got_unc.starts_with(r"\\?\UNC\server\share\"),
            "got {got_unc:?}"
        );
    }

    /// 超长路径提示：只在超阈值时给出，且含人话根因
    #[test]
    fn long_path_hint_only_when_long() {
        assert!(long_path_hint(r"C:\tmp\db.sqlite").is_none());
        let long = format!(r"C:\{}", "a\\".repeat(130));
        let hint = long_path_hint(&long).expect("超长路径必须给提示");
        assert!(hint.contains("260"));
        // 文案不说"超过"：240-259 区间只是接近上限，不断言不成立的根因
        assert!(!hint.contains("超过"), "got: {hint}");
        // 长度按字符计：含中文的路径字节数远大于字符数
        let cjk = format!(r"C:\{}\db.sqlite", "目录\\".repeat(40));
        let hint2 = long_path_hint(&cjk).expect("含中文长路径必须给提示");
        let chars: usize = cjk.chars().count();
        assert!(
            hint2.contains(&chars.to_string()),
            "须按字符数 {chars} 报告: {hint2}"
        );
    }

    /// 真实打开验证（端到端）：用规范化后的 verbatim 路径 Database::open
    /// 必须能在 >260 字符的目录里建库成功（修复前 rusqlite 报
    /// unable to open database file；实测复现 2026-09-25）。
    #[test]
    fn open_long_path_db_end_to_end() {
        let base = std::env::temp_dir();
        let mut dir = base.clone();
        // 逐级加深直到总路径 > 260 字符
        let seg = "kyn-longpath-probe-level-xxxxxxxx";
        let mut depth = 0;
        while dir.to_string_lossy().len() <= 260 && depth < 20 {
            dir = dir.join(seg);
            depth += 1;
        }
        let db = dir.join("kynoptic.db");
        assert!(
            db.to_string_lossy().len() > 260,
            "测试前置：路径须超 260 字符"
        );
        let opened = Database::open(&db.to_string_lossy());
        match opened {
            Ok(dbh) => {
                dbh.mark_stopping();
            }
            Err(e) => panic!("超长路径建库失败（normalize 未生效？）: {e}"),
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 云同步路径检测：路径组件命中 OneDrive 即告警位命中；普通本地文件不命中
    /// （CLOUD_FILE_ATTRIBUTE=0x400000 在真机探针验证：普通 %TEMP% 文件不带该位）。
    #[test]
    fn cloud_sync_path_detected_by_component() {
        let base = std::env::temp_dir().join(format!("kyn-cloud-probe-{}", std::process::id()));
        let dir = base.join("OneDrive");
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("t.db");
        std::fs::write(&f, b"x").unwrap();
        assert!(is_cloud_sync_path(&f), "OneDrive 路径组件必须命中");

        let plain = std::env::temp_dir().join(format!("kyn-not-cloud-{}.db", std::process::id()));
        std::fs::write(&plain, b"x").unwrap();
        assert!(!is_cloud_sync_path(&plain), "普通本地文件不得命中");

        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::remove_file(&plain);
    }

    /// WAL 水位对账：水位不高于当前 max(rowid) 时正常；回退即报告（WAL 静默
    /// 蒸发防护，见 reconcile_events_watermark）。
    #[test]
    fn watermark_regression_detected() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE events(x INTEGER PRIMARY KEY, v TEXT); \
             CREATE TABLE metadata(key TEXT PRIMARY KEY, value TEXT);",
        )
        .unwrap();
        for _ in 1..=5 {
            conn.execute("INSERT INTO events(v) VALUES ('e')", [])
                .unwrap();
        }
        // 首次对账：建立水位 5，无回退
        assert!(reconcile_events_watermark(&conn).is_none());
        let wm: String = conn
            .query_row(
                "SELECT value FROM metadata WHERE key = 'events_watermark_max_rowid'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(wm, "5");
        // 模拟已提交行蒸发：当前 max(rowid) 回退到 2，持久化水位仍为 5
        conn.execute("DELETE FROM events WHERE x > 2", []).unwrap();
        let msg = reconcile_events_watermark(&conn).expect("回退必须被发现");
        assert!(msg.contains("回退"), "告警文本须含回退: {msg}");
    }
}
