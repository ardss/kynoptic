use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crossbeam_channel::bounded;
use rand::Rng;

use crate::constants;
use crate::db::Database;
use crate::input_agg;
use crate::monitors;
use crate::types::{Event, EventHook, Monitor};

/// 通道满载丢弃计数（审查 33-F4：与未来时间戳拒绝拆分——两者此前共用一个
/// 计数器，看门狗对两者都打「通道已满」，无法区分丢数原因）。
static CHANNEL_DROPPED: AtomicU64 = AtomicU64::new(0);

/// 未来时间戳拒绝计数（reject_future_events 专用，口径独立告警）。
static FUTURE_REJECTED: AtomicU64 = AtomicU64::new(0);

/// 数据库写失败累计（P0）：磁盘写满/写失败时旧实现只在 db 层 log（"降级逐条
/// → 跳过"后无任何观测），而 DropWatchdog 只看 DROPPED_EVENTS（通道满载），
/// 写失败完全静默。db/events.rs 的所有最终失败写路径（整批失败、单条降级
/// 失败、提交失败）都累加此计数，由看门狗周期性读取并告警。
pub(crate) static WRITE_FAILURES: AtomicU64 = AtomicU64::new(0);

/// 看门狗连续观察到写失败的周期数（>=3 升级 log::error）。
static CONSECUTIVE_WRITE_FAILURE_PERIODS: AtomicU64 = AtomicU64::new(0);

/// 最近一次成功落库（write_batch 事务提交）的 unix 秒（0 = 尚未写过）。
///
/// 审查 P1：tray 心跳此前与采集健康完全解耦——采集主循环挂死时心跳线程
/// 仍在每 30s 刷新文件，watchdog 的"心跳新鲜 = 采集健康"判定被架空。此
/// 原子量是采集侧唯一可信的"我真的在写库"信号：writer 线程每次成功事务
/// 后更新；tray 心跳线程读取它并在停滞超阈值时在心跳内容里打 stalled 标
/// （消费方在 crates/cli/src/main.rs classify_heartbeat）。跨 crate 无法人
/// 手 Collector 实例，故用进程级静态量（tray 与采集器同进程）。
static LAST_FLUSH_EPOCH: AtomicU64 = AtomicU64::new(0);

/// 最近一次成功落库的 unix 秒（0 = 本进程尚未写过任何批次）。
pub fn last_flush_epoch() -> u64 {
    LAST_FLUSH_EPOCH.load(Ordering::Relaxed)
}

pub(crate) fn send_event(tx: &crossbeam_channel::Sender<Event>, event: Event) -> bool {
    match tx.try_send(event) {
        Ok(()) => true,
        // 只计"真满"：Disconnected 表示采集器已关停（writer 已退出），
        // 属关停尾部的一次性发送，计入丢弃只会污染后续会话的观测。
        Err(crossbeam_channel::TrySendError::Full(_)) => {
            CHANNEL_DROPPED.fetch_add(1, Ordering::Relaxed);
            false
        }
        Err(crossbeam_channel::TrySendError::Disconnected(_)) => false,
    }
}

/// 看门狗单次检查（纯逻辑，便于测试升级策略）：通道丢弃与写失败任一增量 >0
/// 都打 log::warn（带数值与类型）；写失败连续 3 个周期出现则升级 log::error。
/// 返回更新后的连续写失败周期数。
/// Wave22 P1：写失败升级为 error 时同步留档到 data 目录（磁盘满场景下
/// tray.log 同样写不进；恢复后第一份错误可留痕）。
/// 看门狗写失败留档（有界日志纪律，对齐 crates/tray/src/filelog.rs 的做法）：
/// 日志类文件必须有界——单文件 1MB 上限，超限轮转一次成 collector-error.log.old
/// （覆盖式，总占用 ≤ 2MB）。此前追加写无上限：磁盘写满类永久故障下看门狗每
/// 60s 留档一次，反而加速吃满磁盘。轮转失败退化为截断（宁可丢旧错误留痕，
/// 不能让日志文件无限膨胀）。
/// db 层的启动期异常（空库文件/云同步目录检测）也复用本函数留档。
pub(crate) fn archive_write_failure(msg: &str) {
    use std::io::Write;
    const MAX_BYTES: u64 = 1024 * 1024;
    let Some(dir) = crate::db::resolve_db_path()
        .parent()
        .map(|p| p.to_path_buf())
    else {
        return;
    };
    let path = dir.join("collector-error.log");
    // 先查大小再写：超限先轮转，保证本轮错误记录落在新文件里
    if std::fs::metadata(&path)
        .map(|m| m.len() >= MAX_BYTES)
        .unwrap_or(false)
    {
        let old = dir.join("collector-error.log.old");
        let _ = std::fs::remove_file(&old);
        // 打开中的句柄/杀毒扫描短暂持有文件会令 rename 失败：短重试几次，
        // 仍失败则截断（同 filelog.rs 的降级策略）
        let mut done = false;
        for _ in 0..5 {
            if std::fs::rename(&path, &old).is_ok() {
                done = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        if !done {
            let _ = std::fs::File::create(&path);
        }
    }
    let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    else {
        return;
    };
    let _ = writeln!(f, "[{}] {}", chrono::Utc::now().to_rfc3339(), msg);
}

fn watchdog_tick(
    chan_dropped: u64,
    future_rejected: u64,
    write_failures: u64,
    consecutive_wf: &mut u64,
) -> u64 {
    // 审查 33-F4：两类丢弃分别告警，不再统一打成「通道已满」
    if chan_dropped > 0 {
        log::warn!("通道已满，过去 60 秒丢弃了 {} 个事件", chan_dropped);
    }
    if future_rejected > 0 {
        log::warn!(
            "过去 60 秒拒绝了 {} 条未来时间戳事件（> UTC now+5min），不入库",
            future_rejected
        );
    }
    if write_failures > 0 {
        *consecutive_wf += 1;
        let msg = format!(
            "数据库写失败，过去 60 秒新增 {} 次（连续 {} 个周期）",
            write_failures, *consecutive_wf
        );
        if *consecutive_wf >= 3 {
            log::error!("{}", msg);
            archive_write_failure(&msg);
        } else {
            log::warn!("{}", msg);
        }
    } else {
        *consecutive_wf = 0;
    }
    *consecutive_wf
}

/// 丢弃/写失败计数的唯一消费方：看门狗线程每 60 秒 swap 一次（三个计数器）。
fn log_dropped_events_watchdog() {
    let chan_dropped = CHANNEL_DROPPED.swap(0, Ordering::Relaxed);
    let future_rejected = FUTURE_REJECTED.swap(0, Ordering::Relaxed);
    let write_failures = WRITE_FAILURES.swap(0, Ordering::Relaxed);
    let mut consecutive = CONSECUTIVE_WRITE_FAILURE_PERIODS.load(Ordering::Relaxed);
    watchdog_tick(
        chan_dropped,
        future_rejected,
        write_failures,
        &mut consecutive,
    );
    CONSECUTIVE_WRITE_FAILURE_PERIODS.store(consecutive, Ordering::Relaxed);
}

/// flush 决策（纯函数，便于测试）。
///
/// P0 写放大修复：本批是否含 InputAgg 事件**不再**参与决策。InputAgg 每秒
/// 入队一次，旧实现"见 Agg 即提交"把提交节奏拉高到每秒一次事务（每次约 38KB
/// WAL 页写）。UPSERT 是整行覆盖语义（0005 迁移），跟随常规批量节奏（batch 满
/// 或 flush_interval 到期）不会丢数——后写覆盖先写，终值正确。
fn should_flush(
    batch_len: usize,
    elapsed_since_flush: Duration,
    batch_size: usize,
    flush_interval: Duration,
) -> bool {
    batch_len >= batch_size || (batch_len > 0 && elapsed_since_flush >= flush_interval)
}

fn rand_jitter() -> f64 {
    rand::thread_rng().gen_range(-1.0..=1.0)
}

// ─── 采集器设置 ───────────────────────────────────────────────────────────────

/// 输入事件（键盘/鼠标 Hook）的存储粒度。
///
/// **默认 [`InputGranularity::Minute`]（计数制）**：输入折叠为每分钟每桶一行
/// `input_agg` 计数型事件（见 [`crate::input_agg`]）。这是隐私边界设计而非
/// 数据裁剪——SOP 阶段四监控类项目硬标准："键盘类数据只存计数不存内容，
/// 这是与 spyware 划清界限的核心证据"。计数语义（APM/活跃分钟/daily_agg）
/// 完整保留。
///
/// [`InputGranularity::Raw`] 为 opt-in：逐事件原样落库（含按键明细与鼠标
/// 坐标），供明确知情、需要明细的用户在设置中显式开启。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InputGranularity {
    /// 每分钟每桶一行计数型事件（默认；隐私边界：只存计数不存内容）
    #[default]
    Minute,
    /// 逐事件原样落库（opt-in：含按键明细与鼠标坐标）
    Raw,
}

/// 采集器运行设置（代码内默认值；默认值调整属产品决策，见 CODE_NOTES.md §8）。
#[derive(Debug, Clone, Copy)]
pub struct CollectorSettings {
    /// 输入事件存储粒度（默认 Raw）
    pub input_granularity: InputGranularity,
    /// writer 小批 flush 间隔秒数（默认 30s）。数值是"落库最大延迟"与
    /// "每提交 WAL 页开销主导的磁盘足迹"之间的权衡，仍在校准中。
    pub write_flush_interval_secs: u64,
    /// per-key（VK）键频记录开关（默认 true：本地数据完整优先）。
    ///
    /// true（默认）时 input_agg 的 keyboard 行带 `vk` 频次 map（WhatPulse 式
    /// 热力图数据源，只存每键次数不存内容）；false 为显式 opt-out，只累计
    /// keys 总数，`vk` map 输出为空。
    ///
    /// 接线点（由 tray/CLI 侧代理完成）：设置界面 / CLI flag 读写此字段，
    /// 字段名 `vk_frequency_enabled`，经 `CollectorSettings` 传入
    /// `start_collection_with` / `start_collection_custom` 即生效。
    pub vk_frequency_enabled: bool,
}

impl Default for CollectorSettings {
    fn default() -> Self {
        Self {
            input_granularity: InputGranularity::default(),
            write_flush_interval_secs: constants::WRITE_FLUSH_INTERVAL_SECS,
            vk_frequency_enabled: true,
        }
    }
}

fn create_monitors_for(
    enabled: &std::collections::HashSet<String>,
) -> Vec<Box<dyn Monitor + Send>> {
    crate::registry::create_monitors_for(enabled)
}

/// 未来时间戳防线（Wave30 挂账，根治读侧修补之外的写入源）：拒绝 timestamp
/// 晚于当前 UTC 时间 5 分钟以上的事件（时钟漂移/系统时间回拨/外部注入），
/// 计数进 DROPPED_EVENTS 并留日志——趋势/热力图永远不会再画出未来柱。
/// 铁律：只拦截新入库，历史数据不清洗；不可解析的时间戳不在写入侧拦截
/// （交由 db/events.rs 的 normalize_timestamp 与读侧容错处理）。
fn reject_future_events(batch: &mut Vec<Event>) {
    let cutoff = chrono::Utc::now() + chrono::Duration::minutes(5);
    let before = batch.len();
    batch.retain(
        |e| match chrono::DateTime::parse_from_rfc3339(&e.timestamp) {
            Ok(t) => t.with_timezone(&chrono::Utc) <= cutoff,
            Err(_) => true,
        },
    );
    let dropped = before - batch.len();
    if dropped > 0 {
        FUTURE_REJECTED.fetch_add(dropped as u64, Ordering::Relaxed);
        log::warn!("拒绝 {dropped} 条未来时间戳事件（> UTC now+5min），不入库");
    }
}

/// 落库一批事件，返回是否成功（整批全失败时为 false）。
///
/// 结构性修复（写失败路径）：insert 层现在**显式**返回「本批全失败」布尔
/// （db/events.rs，与 input_agg 合法的 rowid=0 严格区分），全失败时——
/// 1. 不推进 LAST_FLUSH_EPOCH（心跳/停滞看门狗保持真实）；
/// 2. 不累加 total_written（写入统计不虚增）；
/// 3. 调用方（writer_loop_inner）**保留 batch 原地重试**并背压：不再无条件
///    clear 把已 pop 的事件丢掉——磁盘满/锁库恢复后本批仍能落库；重试期间
///    writer 完全停止从通道 pop（收集循环有 batch_size 容量前置判断，等待
///    分支也不在满批时 recv），通道被填满后新事件在 send 端计数丢弃
///    （CHANNEL_DROPPED，看门狗可见），batch 本身不无界增长，符合
///    「绝不丢已确认数据」与有界内存两条铁律；重试按指数退避（500ms 起,
///    上限 8s）避免对整批反复做事务尝试。
fn write_batch(db: &Database, batch: &mut Vec<Event>, total_written: &AtomicUsize) -> bool {
    // 未来时间戳防线：先于 session 补盖与落库执行（见 reject_future_events）
    reject_future_events(batch);
    if batch.is_empty() {
        return true;
    }
    // 审查 P0：Event::new 硬编码 session_id=None 且全链路无人回填，导致
    // events.session_id 全库为 NULL、sessions.total_events/ghost 清扫失效。
    // 落库前用 db 登记的当前会话 id 补盖（None 才盖，尊重显式赋值）。
    // 审查 P2：改为就地补盖（按 &mut 拿 batch），不再 clone 整个 Vec。
    let sid = db.current_session_id();
    if sid != 0 {
        for e in batch.iter_mut() {
            if e.session_id.is_none() {
                e.session_id = Some(sid);
            }
        }
    }
    // 聚合增量维护与 events 落库在同一事务内完成（审查 P1：两个独立事务之间
    // kill 会留下"events 有 agg 无"的欠聚合且永不自愈）；rowids 与 batch 一一
    // 对应（失败行为 0），事务化后由 insert 层内部直接用于 agg 维护。
    let (rowids, all_failed) = db.insert_events_with_agg(batch);
    let landed = !all_failed && rowids.len() == batch.len();
    if landed {
        total_written.fetch_add(batch.len(), Ordering::Relaxed);
        // 审查 P1：事务成功即刷新"最近落库"时钟，供 tray 心跳判定采集是否停滞
        LAST_FLUSH_EPOCH.store(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            Ordering::Relaxed,
        );
    } else {
        // 整批全失败（磁盘满/库锁）：计一次失败、不推进心跳时钟与写入统计。
        // batch 由调用方保留重试（背压语义见函数头注释）。
        WRITE_FAILURES.fetch_add(1, Ordering::Relaxed);
        log::error!(
            "整批落库失败，{} 条事件保留待重试，心跳时钟不推进",
            batch.len()
        );
    }
    landed
}

fn writer_loop(
    rx: crossbeam_channel::Receiver<Event>,
    db: Arc<Database>,
    batch_size: usize,
    flush_interval: Duration,
    total_written: Arc<AtomicUsize>,
    // 退出条件已改为只认通道断开（生产者随停机旗标退出后自然 Disconnected）
    _shutdown: Arc<AtomicBool>,
    // 关停兜底：聚合线程 join 后由 shutdown 置位，writer 排空尾批退出，
    // 不再无限等待可能卡死的生产者断开通道（审查 P1）
    writer_stop: Arc<AtomicBool>,
) {
    use std::panic;

    // batch 放在重启循环这一层（审查 P2）：inner panic 时 batch 里可能有
    // 未落库事件，重启后先 flush 残余再继续，不让 panic 丢整批。
    let mut batch: Vec<Event> = Vec::with_capacity(batch_size);
    loop {
        let result = panic::catch_unwind(panic::AssertUnwindSafe(|| {
            writer_loop_inner(
                &rx,
                &db,
                batch_size,
                flush_interval,
                &total_written,
                &mut batch,
                &writer_stop,
            )
        }));
        match result {
            Ok(Some(flush_count)) => {
                log::info!("Writer 正常退出，已 flush {} 条", flush_count);
                return;
            }
            Ok(None) => continue,
            Err(e) => {
                // 残余 batch 先落库（panic 可能发生在 flush 之前的任意点）
                if !batch.is_empty() {
                    log::warn!("Writer panic 后 flush 残余 batch {} 条", batch.len());
                    let landed = write_batch(&db, &mut batch, &total_written);
                    // 只在成功时清空；失败保留事件随重启循环继续重试（背压语义）
                    if landed {
                        batch.clear();
                    }
                }
                log::error!("Writer 线程 panic: {:?}，1 秒后重启", e);
                thread::sleep(Duration::from_secs(1));
            }
        }
    }
}

fn writer_loop_inner(
    rx: &crossbeam_channel::Receiver<Event>,
    db: &Arc<Database>,
    batch_size: usize,
    flush_interval: Duration,
    total_written: &AtomicUsize,
    batch: &mut Vec<Event>,
    stop_flag: &std::sync::atomic::AtomicBool,
) -> Option<usize> {
    use crossbeam_channel::{RecvTimeoutError, TryRecvError};
    use std::time::Instant;

    let mut last_flush = Instant::now();
    // 持久写失败的指数退避（500ms 起,上限 8s）:整批保留重试时避免每个
    // 500ms 窗口都对整批重复 reject/session 补盖/事务尝试;成功即复位。
    let mut fail_backoff = Duration::from_millis(500);

    loop {
        // 审查 P1：writer 退出不能只认通道 Disconnected——监控线程持 tx clone
        // 且可能被卡死（如 clipboard OpenClipboard 被他进程长占），join 永远
        // 等不到 Disconnected，shutdown 整体挂死。writer_stop 在聚合线程
        // join（终值 flush 已入队）之后由 shutdown 置位，writer 到此做最终
        // 排空落库再退出，不丢尾批。
        if stop_flag.load(Ordering::Acquire) {
            while let Ok(event) = rx.try_recv() {
                batch.push(event);
            }
            let n = batch.len();
            if n > 0 {
                write_batch(db, batch, total_written);
            }
            return Some(n);
        }
        loop {
            // 容量前置判断:写失败保留重试期间 batch 已满,绝不再从通道 pop,
            // 让通道填满、send 端走 CHANNEL_DROPPED 背压（batch 不无界增长）。
            if batch.len() >= batch_size {
                break;
            }
            match rx.try_recv() {
                Ok(event) => {
                    batch.push(event);
                    if batch.len() >= batch_size {
                        break;
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    let n = batch.len();
                    if n > 0 {
                        write_batch(db, batch, total_written);
                    }
                    return Some(n);
                }
            }
        }

        // flush 决策只看 batch 满与时间窗；InputAgg 不再触发即时提交（见
        // should_flush 文档）。
        let now = Instant::now();
        if should_flush(
            batch.len(),
            now.duration_since(last_flush),
            batch_size,
            flush_interval,
        ) {
            // 整批失败时保留 batch 原地重试（写失败背压，见 write_batch 文档）：
            // batch 恒满，收集循环与等待分支都不再 pop（通道填满后 send 端
            // 计数丢弃），指数退避后下一轮整批重试——不再无条件 clear 丢批，
            // batch 也不随失败时长无界增长。
            let landed = write_batch(db, batch, total_written);
            if landed {
                batch.clear();
                last_flush = now;
                fail_backoff = Duration::from_millis(500);
            } else {
                thread::sleep(fail_backoff);
                fail_backoff = (fail_backoff * 2).min(Duration::from_secs(8));
            }
        }

        if batch.is_empty() {
            // 审查 P1：退出只认通道断开（所有生产者退出后 Disconnected）。
            // 不能 batch 空+shutdown 就抢跑——聚合线程的关停终值 flush
            // 会撞上已断开的通道被静默丢弃。shutdown 前会 join 全部生产者。
            match rx.recv_timeout(flush_interval.min(Duration::from_millis(500))) {
                Ok(event) => batch.push(event),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    let n = batch.len();
                    if n > 0 {
                        write_batch(db, batch, total_written);
                    }
                    return Some(n);
                }
            }
        } else {
            // batch 非空但未到 flush 时机：阻塞等待剩余时间（或新事件），
            // 而不是回到 try_recv 空转。旧实现在此直接继续外层循环 →
            // 只要 batch 里有任何待 flush 事件，writer 线程就以 ~100% 单核
            // 空转直到 flush_interval 到期（perf-idle 基准 2026-09 实测的
            // 空载 CPU 根因）。
            let until_flush = flush_interval
                .saturating_sub(now.duration_since(last_flush))
                .min(Duration::from_millis(500));
            // 满批（写失败保留重试中）时不得再 recv:recv 出来既装不下也不
            // 能丢,直接睡到重试点,通道留给 send 端背压。
            if batch.len() >= batch_size {
                thread::sleep(until_flush);
                continue;
            }
            match rx.recv_timeout(until_flush) {
                Ok(event) => batch.push(event),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    let n = batch.len();
                    if n > 0 {
                        write_batch(db, batch, total_written);
                    }
                    return Some(n);
                }
            }
        }
    }
}

fn run_monitor(
    m: Box<dyn Monitor + Send>,
    tx: crossbeam_channel::Sender<Event>,
    shutdown: Arc<AtomicBool>,
) {
    let name = m.name().to_string();
    let interval = m.interval();

    if let Err(e) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        m.collect(&tx);
    })) {
        log::warn!("{} 首次采集 panic: {:?}，继续重试", name, e);
    } else {
        log::info!("{} 首次采集完成", name);
    }

    loop {
        if shutdown.load(Ordering::Acquire) {
            return;
        }

        if let Err(e) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            m.collect(&tx);
        })) {
            log::error!("{} 采集 panic: {:?}，5 秒后重试", name, e);
            thread::sleep(Duration::from_secs(5));
            continue;
        }

        let base = interval.as_secs_f64();
        let jitter_factor = 1.0 + rand_jitter() * 0.3;
        let wait = Duration::from_secs_f64(base * jitter_factor);

        let deadline = std::time::Instant::now() + wait;
        // 电池修复（审查 P0）：旧实现 200ms 切片轮询 deadline，12 个监控线程
        // × 5 次/秒 = 70+ 次无谓唤醒。改为按剩余时长整段 sleep（上限 1 秒），
        // 关停语义仍保证：shutdown 置位后线程最多 1 秒内检查到旗标退出。
        while std::time::Instant::now() < deadline {
            if shutdown.load(Ordering::Acquire) {
                return;
            }
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            thread::sleep(remaining.min(Duration::from_secs(1)));
        }
    }
}

pub struct Collector {
    pub db: Arc<Database>,
    pub session_id: i64,
    pub total_written: Arc<AtomicUsize>,
    pub writer_handle: Option<thread::JoinHandle<()>>,
    /// InputAgg 聚合线程句柄：关停时先 join 它（终值 flush 入队）再 join
    /// writer，顺序保证终值必被落库（审查 P1：此前句柄即弃，flush 撞上
    /// writer 已退出的通道被静默丢弃）。
    agg_handle: Option<thread::JoinHandle<()>>,
    /// 定期维护线程句柄（审查 LOW）：shutdown 时置位停机旗标后 join——否则
    /// 热重载后旧 Maintenance 线程可能与新采集器的维护线程并发跑
    /// db.maintenance()。用 Option + take() 保证 shutdown 与 Drop 不双重 join。
    maintenance_handle: Option<thread::JoinHandle<()>>,
    /// 丢弃/写失败看门狗线程句柄（同上，shutdown 时 join）。
    watchdog_handle: Option<thread::JoinHandle<()>>,
    /// 监控线程句柄（审查 33-F7：此前 spawn 后句柄即弃，shutdown 排空 rx_tail
    /// 与 end_session 完成后，尚未查到停机旗标的监控线程仍可做最后一次
    /// send_event——事件进无人消费的通道随 Collector 销毁静默丢失且不可观测
    /// （send_event 的 Disconnected 分支不计数）。shutdown 在排空兜底之前
    /// 全部 join（run_monitor 的睡眠是 1 秒切片，join 有界）。
    monitor_handles: Vec<thread::JoinHandle<()>>,
    /// 已收尾旗标（审查 33-F3）：shutdown() 置位，Drop 据此判「是否需要兜底
    /// 收尾」。此前以 writer_handle 存在性作判据——wait() 先 take 掉句柄后
    /// 仅 join 不收尾，随后 Drop 误判「已收尾」留下 open 幽灵 session。
    finished: bool,
    pub hooks: Vec<Box<dyn EventHook>>,
    /// 设置副本：shutdown 时决定是否 flush 未满分钟的部分输入计数。
    settings: CollectorSettings,
    /// 本实例的停机旗标（每 Collector 一份：probe 会在同进程多次启停采集器，
    /// 全局静态旗标会把上一个实例的监控线程"复活"成僵尸）。
    shutdown: Arc<AtomicBool>,
    /// writer 关停兜底旗标：聚合线程 join 后置位，writer 排空尾批退出
    /// （监控线程卡死时不再挂死 shutdown）。
    writer_stop: Arc<AtomicBool>,
    /// 通道接收端尾柄（审查 P2）：writer 排空退出后，迟到的生产者（关停
    /// 竞态下尚未退出的 hook/monitor 最后一次 collect）仍可能把事件送入
    /// 通道而无人消费。shutdown join writer 后用它做最后一次排空落库。
    rx_tail: Option<crossbeam_channel::Receiver<Event>>,
}

impl Collector {
    /// 停机旗标的克隆：供外部线程（CLI Ctrl+C handler）置位。
    pub fn shutdown_flag(&self) -> Arc<AtomicBool> {
        self.shutdown.clone()
    }

    /// 阻塞直到 writer 线程退出。审查 33-F3：此前只 join writer、不置
    /// writer_stop 不收尾——writer 的退出条件（writer_stop/通道断开）都不
    /// 一定满足，join 可能永久阻塞，即便返回也会让随后的 Drop 误判「已收尾」
    /// 留下幽灵 session。现委托给完整 shutdown()（幂等：Drop 据 finished
    /// 旗标跳过重复收尾）。
    pub fn wait(&mut self) {
        self.shutdown();
    }

    pub fn shutdown(&mut self) {
        if self.finished {
            return;
        }
        self.shutdown.store(true, Ordering::Release);
        // 审查 MEDIUM：通知 db 层停机——maintenance() 据此跳过 checkpoint/VACUUM
        // 等持写互斥体的重活，避免 shutdown 对 Maintenance 线程的 join 被
        // 大库 VACUUM 阻塞数分钟（心跳停滞 1800s 后被 watchdog 误杀）。
        self.db.mark_stopping();

        for h in &self.hooks {
            h.stop();
        }

        // 审查 33-F7：先 join 全部监控线程（旗标已置位，睡眠 1 秒切片 → join
        // 有界），再进聚合/writer 的关停链——保证排空 rx_tail 兜底时所有
        // 生产者都已退出，最后一次 send_event 不会落入无人消费的通道。
        for h in self.monitor_handles.drain(..) {
            let _ = h.join();
        }

        // minute 粒度的"最后一分钟不丢"由聚合线程负责：先 join 它（终值
        // flush 已入队），再 join writer（审查 P1：顺序不能反）。
        if let Some(h) = self.agg_handle.take() {
            let _ = h.join();
        }
        // writer_stop 置位必须在聚合线程 join 之后：此时所有该落库的尾批
        // （含终值 flush）都已入队，writer 排空退出即可。
        self.writer_stop
            .store(true, std::sync::atomic::Ordering::Release);
        if let Some(handle) = self.writer_handle.take() {
            let _ = handle.join();
        }
        // 审查 LOW：writer 退出后 join 维护与看门狗线程。两个循环的睡眠都做
        // 了 1 秒切片并在切片间检查停机旗标（见 spawn 处），故空闲时的 join
        // 有界（最多 ~1s）；但若 Maintenance 已在执行 db.maintenance()，其
        // checkpoint/VACUUM 曾会持写互斥体阻塞 join 数分钟——已通过
        // shutdown 开头的 db.mark_stopping() 让 maintenance() 跳过重活
        // （见 db/mod.rs），join 不再被 VACUUM 拖长。take() 防止 Drop 路径
        // 双重 join。
        if let Some(h) = self.maintenance_handle.take() {
            let _ = h.join();
        }
        if let Some(h) = self.watchdog_handle.take() {
            let _ = h.join();
        }
        // 终值直写兜底（与 Drop 同款，审查 P1）：聚合线程的关停终值 flush 走
        // 通道，若通道已满（writer 停滞场景）会被 try_send 丢弃且无人补偿。
        // writer 退出后直写最后一份 partial，保证"最后一分钟不丢"语义成立。
        // 先排空通道残留（见下），partial 终值必须后落库才能覆盖旧快照。
        // 审查 P2：writer 已排空退出，但迟到的生产者（关停竞态下 hook/monitor
        // 尚未退出的最后一次 collect/send）可能仍在通道里留下事件——此后无人
        // 消费即永久丢失。join 之后用保留的接收端尾柄做最后一次排空落库；
        // 写失败已由 insert 层计入 WRITE_FAILURES（DropWatchdog 告警）。
        if let Some(rx) = self.rx_tail.take() {
            let mut tail: Vec<Event> = Vec::new();
            while let Ok(ev) = rx.try_recv() {
                tail.push(ev);
            }
            if !tail.is_empty() {
                log::info!("writer 退出后排空通道残留 {} 条", tail.len());
                write_batch(&self.db, &mut tail, &self.total_written);
            }
        }
        if self.settings.input_granularity == InputGranularity::Minute {
            // 审查 33-F1 第二道防线：当前分钟行已是上一会话的完整终值
            // （$.final=true）时，直写 partial 会把大终值整行覆盖回本次重启
            // 会话的几键小值——跳过（正常路径下 PendingFlush::take 已保留
            // 抑制语义，此处兜底抑制失效场景，如同分钟内热重载后抑制被消费）。
            if !current_minute_row_is_final(&self.db) {
                let mut events = input_agg::flush_partial(chrono::Local::now());
                if !events.is_empty() {
                    write_batch(&self.db, &mut events, &self.total_written);
                }
            }
        }

        // 审查 33-F6：total_events 统一为 COUNT(*) 口径（与 ghost 清扫
        // sessions.rs 的 COUNT(*) 一致）。此前优雅关停写 total_written——把
        // 每秒一次的 input_agg UPSERT 逐次累加，同一会话因结束方式不同可差
        // 约 60 倍。total_written 仅保留给写入吞吐观测（CLI collected 输出）。
        let total = self.db.session_event_count(self.session_id);
        // 审查：空转时长此前硬编码 0.0，会话内 idle_start/idle_end 事件对
        // 覆盖的空转/离机时段从未折算进 sessions.idle_seconds（跨日长会话
        // 统计失真）。与幽灵清扫共用 db 层同一口径回填。
        let idle_secs = self.db.session_idle_seconds(self.session_id);
        self.db.end_session(self.session_id, total, idle_secs);

        self.finished = true;
        log::info!("采集器已停止");
    }
}

/// Drop 兜底：如果 shutdown 之前没被显式调用（例如应用崩溃、强杀、panic），
/// 在析构时仍尝试关闭当前 session，避免幽灵 session 累积。
/// 审查 33-F3：守据改为 finished 旗标（shutdown/wait 都会走完整收尾），
/// 不再以 writer_handle 存在性判「已收尾」——wait() take 句柄后 Drop 会
/// 误判并跳过 end_session，留下 open 幽灵 session。
impl Drop for Collector {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        log::warn!(
            "Collector 被 drop 但未调用 shutdown() — 触发兜底关闭 session {}",
            self.session_id
        );
        self.shutdown();
    }
}

/// 默认设置启动（Raw 粒度 + 30s flush）。需要自定义用 [`start_collection_with`]。
pub fn start_collection(db_path: &str) -> Collector {
    start_collection_with(CollectorSettings::default(), db_path)
}

pub fn start_collection_with(settings: CollectorSettings, db_path: &str) -> Collector {
    let enabled: std::collections::HashSet<String> = crate::registry::default_enabled_ids()
        .iter()
        .map(|s| s.to_string())
        .collect();
    start_collection_custom(&enabled, settings, db_path)
}

/// 当前本地分钟在库里的 input_agg 行是否已是**完整终值**（`$.final` 为 true）。
///
/// 审查 P1：重启抑制的判定依据。返回 true（终值已写出）或 false（无行 /
/// 只有部分快照）语义如下：
/// - true → 保持抑制：新会话的秒级小快照会覆盖大终值，必须丢弃；
/// - false 且无行 → 保持抑制（该分钟本来就没有数据，防小值覆盖）；
/// - false 且有行（部分快照，上次会话被硬杀、终值从未写出）→ 解除抑制
///   （合并续写），由调用方据此调用 `input_agg::clear_restart_suppression`。
///
/// 查询失败按 false 处理（保持抑制，宁可丢当前分钟部分计数不冒覆盖风险）。
fn current_minute_row_is_final(db: &Database) -> bool {
    // 与 input_agg 行时间戳同格式（分钟起点 epoch 取整 → UTC RFC3339）
    let now_min = (chrono::Utc::now().timestamp() / 60) * 60;
    let ts = chrono::DateTime::<chrono::Utc>::from_timestamp(now_min, 0)
        .map(|t| t.to_rfc3339())
        .unwrap_or_default();
    let reader = db.reader();
    reader
        .query_row(
            "SELECT COALESCE(json_extract(event_data, '$.final'), 0) FROM events \
             WHERE event_action = 'input_agg' AND timestamp = ?1 \
             ORDER BY id DESC LIMIT 1",
            rusqlite::params![&ts],
            |r| r.get::<_, i64>(0),
        )
        .map(|v| v != 0)
        .unwrap_or(false)
}

/// 以显式启用集启动采集器（probe/诊断用）。
/// 启用集为空或不含 hook id 时对应 Hook 不启动；其余行为与 [`start_collection_with`] 相同。
pub fn start_collection_custom(
    enabled: &std::collections::HashSet<String>,
    settings: CollectorSettings,
    db_path: &str,
) -> Collector {
    env_logger::Builder::from_env("RUST_LOG")
        .filter_level(log::LevelFilter::Info)
        .try_init()
        .ok();

    // 本实例独立的停机旗标（见 Collector.shutdown 字段文档）。
    let shutdown = Arc::new(AtomicBool::new(false));
    // 审查 33-F4：丢弃计数改 swap 取复位前余数（与下方 WRITE_FAILURES 同款）——
    // 热重载时上一实例关停尾部的丢弃发生在旧 watchdog 已 join 之后，无条件
    // store(0) 会静默抹零。非零即 log::error 并留档。
    let prev_chan = CHANNEL_DROPPED.swap(0, Ordering::Relaxed);
    let prev_future = FUTURE_REJECTED.swap(0, Ordering::Relaxed);
    if prev_chan > 0 || prev_future > 0 {
        let msg = format!(
            "上一采集实例遗留 {prev_chan} 条通道满载丢弃、{prev_future} 条未来时间戳拒绝未被看门狗消费（热重载复位前发现）"
        );
        log::error!("{msg}");
        archive_write_failure(&msg);
    }
    // 审查 LOW：热重载不能无条件清零上一实例遗留的写失败——旧实例 shutdown
    // 尾排空的写失败发生在旧 watchdog 已 join 之后，无条件 store(0) 会让它
    // 永久无告警。改 swap 取复位前余数并立即告警（留档口径同看门狗）。
    let prev_write_failures = WRITE_FAILURES.swap(0, Ordering::Relaxed);
    if prev_write_failures > 0 {
        let msg = format!(
            "上一采集实例遗留 {prev_write_failures} 次写失败未被看门狗消费（热重载复位前发现）"
        );
        log::error!("{msg}");
        archive_write_failure(&msg);
    }
    CONSECUTIVE_WRITE_FAILURE_PERIODS.store(0, Ordering::Relaxed);
    // 回归审查 P1：flush epoch 是进程级全局，重启采集器时不清会带着上一
    // 会话的时间戳——心跳线程立即误判 stalled（raw 粒度安静机器上首笔
    // 落库可合法超过 300s），看门狗循环误杀健康托盘。置为当前时刻重启计时。
    LAST_FLUSH_EPOCH.store(
        chrono::Utc::now().timestamp().max(0) as u64,
        std::sync::atomic::Ordering::Relaxed,
    );

    let db = Arc::new(Database::open(db_path).expect("数据库初始化失败"));
    // 启动时清扫上次未关闭的 session（崩溃/强杀留的幽灵）
    db.close_ghost_sessions();
    let session_id = db.start_session();
    log::info!("Session {} 已创建", session_id);

    let (tx, rx) = bounded::<Event>(constants::CHANNEL_CAPACITY);
    // 接收端尾柄：writer 线程拿走 rx，shutdown 在 join writer 之后用这份
    // clone 排空迟到的生产者残留（审查 P2，见 Collector.rx_tail 文档）。
    let rx_tail = rx.clone();
    let total_written = Arc::new(AtomicUsize::new(0));
    // writer 关停兜底旗标：shutdown 在聚合线程 join 之后置位（终值 flush 已
    // 入队），writer 排空尾批退出——不再依赖"全部生产者断开通道"这一可能
    // 永远等不到的条件（监控线程卡死即挂死，审查 P1）。
    let writer_stop = Arc::new(AtomicBool::new(false));

    let db_w = db.clone();
    let tw = total_written.clone();
    let sd_writer = shutdown.clone();
    let ws_writer = writer_stop.clone();

    // 审查 P1：spawn 失败（panic-after-spawn 僵尸采集器修复）后的兜底清理。
    // 构造一个"半成品" Collector 并走其 shutdown()——置停机旗标、停 hooks、
    // join 已有聚合/writer 线程、flush_partial、end_session——保证任意阶段
    // spawn 失败都不留僵尸线程/幽灵 session；panic 本身照常抛出（tray 的
    // catch_unwind 路径不受影响），失败的 spawn 本就没有线程需要回收。
    // （用宏而非闭包：闭包按引用捕获会把 db 借用拖到函数尾，与收尾的
    // Collector { db, .. } 移动冲突。）
    macro_rules! spawn_fail_collector {
        ($writer:expr, $agg:expr, $maint:expr, $watch:expr, $monitors:expr, $hooks:expr) => {{
            Collector {
                db: db.clone(),
                session_id,
                total_written: total_written.clone(),
                writer_handle: $writer,
                agg_handle: $agg,
                maintenance_handle: $maint,
                watchdog_handle: $watch,
                monitor_handles: $monitors,
                finished: false,
                hooks: $hooks,
                settings,
                shutdown: shutdown.clone(),
                writer_stop: writer_stop.clone(),
                // spawn 失败路径没有 writer 线程消费 rx，交给 shutdown 的排空兜底
                rx_tail: Some(rx_tail.clone()),
            }
            .shutdown();
        }};
    }

    let writer_handle = match thread::Builder::new()
        .name("EventWriter".into())
        .spawn(move || {
            writer_loop(
                rx,
                db_w,
                constants::WRITE_BATCH_SIZE,
                Duration::from_secs(settings.write_flush_interval_secs.max(1)),
                tw,
                sd_writer,
                ws_writer,
            );
        }) {
        Ok(h) => h,
        Err(e) => {
            spawn_fail_collector!(None, None, None, None, Vec::new(), Vec::new());
            panic!("Writer 启动失败: {e}");
        }
    };

    let monitors = create_monitors_for(enabled);
    let monitor_count = monitors.len();
    log::info!("正在启动 {} 个 Monitor...", monitor_count);

    // 审查 33-F7：保留监控线程句柄，shutdown 收尾时全部 join
    let mut monitor_handles: Vec<thread::JoinHandle<()>> = Vec::new();
    for m in monitors {
        let tx = tx.clone();
        let sd = shutdown.clone();
        match thread::Builder::new()
            .name(m.name().into())
            .spawn(move || run_monitor(m, tx, sd))
        {
            Ok(handle) => monitor_handles.push(handle),
            Err(e) => {
                // 停机旗标让已启动的 monitor 在下一轮检查点（<=1s）退出，
                // writer_stop 让 writer 排空尾批退出，再关闭 session
                spawn_fail_collector!(
                    Some(writer_handle),
                    None,
                    None,
                    None,
                    monitor_handles,
                    Vec::new()
                );
                panic!("Monitor 线程启动失败: {e}");
            }
        }
    }

    // 输入粒度：minute 模式下 Hook 回调退化为原子计数，由独立聚合线程每秒
    // drain 并按分钟折叠成 input_agg 事件入队（见 input_agg 模块文档）。
    input_agg::reset();
    // per-key 频次开关（默认 true）：必须在 activate/reset 之后、Hook
    // 启动之前设置，保证本会话从第一个键事件起口径一致。
    input_agg::set_vk_enabled(settings.vk_frequency_enabled);
    let minute_mode = settings.input_granularity == InputGranularity::Minute;
    let mut agg_handle: Option<thread::JoinHandle<()>> = None;
    if minute_mode {
        input_agg::activate();
        // 审查 P1：硬杀后的部分分钟快照修复。重启抑制只应丢弃"当前分钟已有
        // 完整终值（$.final: true）或没有行"的会话重启小快照；若库里该分钟
        // 只有部分快照（final 非 true，上次会话被强杀、终值从未写出），
        // 抑制会把那部分计数整体丢掉——改为合并续写（解除抑制）。
        if !current_minute_row_is_final(&db) {
            input_agg::clear_restart_suppression();
            log::info!("上一会话在当前分钟留有部分快照，重启抑制改为合并续写");
        }
        let tx_agg = tx.clone();
        let sd_agg = shutdown.clone();
        agg_handle = Some(
            match thread::Builder::new()
                .name("InputAgg".into())
                .spawn(move || {
                    // panic 防护（审查 P2）：drain/flush panic 不允许终结聚合
                    // 线程——catch_unwind 包住单轮工作，Err 则延迟后重启循环。
                    loop {
                        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            thread::sleep(Duration::from_secs(1));
                            if sd_agg.load(Ordering::Acquire) {
                                // 关停兜底（审查 P1：必须在这里入队而不是 shutdown 直写——
                                // 本线程是最后一个生产者，入队晚于队列里残留的秒级小快照，
                                // writer 按序落库即天然消除"小快照后写覆盖最终行"竞态）
                                // 审查 HIGH：flush_partial 是消费性的——撞满通道被丢后
                                // shutdown 的直写兜底会拿到空状态（最后一分钟归零）。
                                // 改用可回滚的 PendingFlush：入队失败即整体还回状态，
                                // shutdown 兜底 flush_partial 仍拿得到最后一分钟计数。
                                let (evts, snap) =
                                    input_agg::PendingFlush::take(chrono::Local::now());
                                let mut all_queued = true;
                                for e in evts {
                                    if !send_event(&tx_agg, e) {
                                        all_queued = false;
                                    }
                                }
                                if !all_queued {
                                    log::warn!("关停终值 flush 撞满通道，状态已还原，交由 shutdown 直写兜底");
                                    snap.restore();
                                }
                                return true;
                            }
                            for e in input_agg::drain(chrono::Local::now()) {
                                send_event(&tx_agg, e);
                            }
                            false
                        }));
                        match result {
                            Ok(true) => return,
                            Ok(false) => {}
                            Err(e) => {
                                log::error!("InputAgg 线程 panic: {:?}，1 秒后重启", e);
                                thread::sleep(Duration::from_secs(1));
                            }
                        }
                    }
                }) {
                Ok(h) => h,
                Err(e) => {
                    spawn_fail_collector!(
                        Some(writer_handle),
                        None,
                        None,
                        None,
                        monitor_handles,
                        Vec::new()
                    );
                    panic!("InputAgg 聚合线程启动失败: {e}");
                }
            },
        );
    }

    // Hook 不带 name()（EventHook trait 最小面），按启用集条件构建。
    let mut hooks: Vec<Box<dyn EventHook>> = Vec::new();
    if enabled.contains("keyboard_hook") {
        hooks.push(Box::new(monitors::keyboard_hook::KeyboardHook));
    }
    if enabled.contains("mouse_hook") {
        hooks.push(Box::new(monitors::mouse_hook::MouseHook));
    }
    let hook_count = hooks.len();
    log::info!("正在启动 {} 个 EventHook...", hook_count);

    for h in &hooks {
        h.start(tx.clone());
    }

    log::info!(
        "Kynoptic 已启动 ({} 个 Monitor + {} 个 Hook)",
        monitor_count,
        hook_count
    );

    // 启动时即刷新一次 daily_agg 基线（今天 + 昨天），避免维护线程要等
    // MAINTENANCE_INTERVAL_SECS（默认 24h）后才首次建立异常检测的历史均值。
    db.refresh_daily_agg();

    let db_clone = db.clone();
    let sd_maint = shutdown.clone();
    let maint_handle = thread::Builder::new()
        .name("Maintenance".into())
        .spawn(move || {
            // 每 DAILY_AGG_REFRESH_SECS（600s = 10 分钟）刷新一次 daily_agg
            // 派生缓存（图表与实时卡片不能互相矛盾）；
            // 每 MAINTENANCE_INTERVAL_SECS 做一次全量维护（清理/回填等重活）。
            // 睡眠做 1 秒切片并在切片间检查停机旗标（审查 LOW）：shutdown 会
            // join 本线程，整段 sleep 会让 join 挂满一个周期（最长 10 分钟）。
            let mut ticks: u64 = 0;
            let interval = Duration::from_secs(constants::DAILY_AGG_REFRESH_SECS);
            let mut next_tick = std::time::Instant::now() + interval;
            loop {
                if sd_maint.load(Ordering::Acquire) {
                    return;
                }
                let now = std::time::Instant::now();
                if now >= next_tick {
                    next_tick = now + interval;
                    ticks += 1;
                    if ticks.is_multiple_of(
                        constants::MAINTENANCE_INTERVAL_SECS / constants::DAILY_AGG_REFRESH_SECS,
                    ) {
                        log::info!("执行定期数据库维护...");
                        db_clone.maintenance();
                    } else {
                        db_clone.refresh_daily_agg();
                    }
                }
                let remaining = next_tick.saturating_duration_since(std::time::Instant::now());
                thread::sleep(remaining.min(Duration::from_secs(1)));
            }
        });
    if let Err(e) = maint_handle {
        // hooks/agg/writer 均已启动：全量兜底清理后再 panic（审查 P1）
        spawn_fail_collector!(
            Some(writer_handle),
            agg_handle,
            None,
            None,
            monitor_handles,
            hooks
        );
        panic!("维护线程启动失败: {e}");
    }

    // 丢弃/写失败看门狗（审查 P2 + P0）：writer 侧日志只在"还在正常收事件"时
    // 可见，写库卡死导致通道持续满载、或磁盘写满导致事件被降级跳过时都会完全
    // 静默。独立线程每 60 秒读一次全局 DROPPED_EVENTS 与 WRITE_FAILURES，
    // 任一增量 >0 即告警（写失败连续 3 个周期升级 error）——与 writer 状态解耦。
    let sd_watch = shutdown.clone();
    let watch_handle = thread::Builder::new()
        .name("DropWatchdog".into())
        .spawn(move || loop {
            // 睡眠 1 秒切片：shutdown join 本线程时最多 ~1 秒退出（审查 LOW）
            for _ in 0..60 {
                if sd_watch.load(Ordering::Acquire) {
                    return;
                }
                thread::sleep(Duration::from_secs(1));
            }
            log_dropped_events_watchdog();
        });
    if let Err(e) = watch_handle {
        spawn_fail_collector!(
            Some(writer_handle),
            agg_handle,
            maint_handle.ok(),
            None,
            monitor_handles,
            hooks
        );
        panic!("丢弃看门狗线程启动失败: {e}");
    }

    Collector {
        db,
        session_id,
        total_written,
        writer_handle: Some(writer_handle),
        agg_handle,
        maintenance_handle: maint_handle.ok(),
        watchdog_handle: watch_handle.ok(),
        monitor_handles,
        finished: false,
        hooks,
        settings,
        shutdown,
        writer_stop,
        rx_tail: Some(rx_tail),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_flush_on_batch_full_or_interval() {
        let interval = Duration::from_secs(30);
        // batch 满 → flush
        assert!(should_flush(300, Duration::from_secs(0), 300, interval));
        // 未满 + 未到期 → 不 flush
        assert!(!should_flush(5, Duration::from_secs(3), 300, interval));
        // 未满但到期 → flush
        assert!(should_flush(5, Duration::from_secs(30), 300, interval));
        // 空 batch 永不 flush
        assert!(!should_flush(0, Duration::from_secs(60), 300, interval));
    }

    /// P0 写放大回归测试：InputAgg 事件每秒入队一次，flush 决策不得因其
    /// 存在而提前（旧实现"见 Agg 即提交"→ 每秒一次事务提交，~38KB WAL/次）。
    #[test]
    fn input_agg_events_do_not_trigger_immediate_flush() {
        // 决策函数签名不含事件内容：含 InputAgg 的小 batch 与其他小 batch
        // 判定完全一致——未满 batch 且未到 flush 窗口时不 flush。
        let interval = Duration::from_secs(30);
        assert!(!should_flush(1, Duration::from_secs(1), 300, interval));
        assert!(!should_flush(60, Duration::from_secs(10), 300, interval));
    }

    /// P0 看门狗升级策略：写失败任一周期 >0 都告警，连续 3 个周期升级 error
    /// （此处断言连续周期计数的推进与清零语义；日志级别由 watchdog_tick 内
    /// 分支选择，error 分支对应返回值 >= 3）。
    #[test]
    fn watchdog_escalates_after_three_consecutive_write_failure_periods() {
        let mut consecutive = 0u64;
        // 无失败：不推进
        assert_eq!(watchdog_tick(0, 0, 0, &mut consecutive), 0);
        // 连续三个周期有写失败：1 → 2 → 3（第 3 周期起 error）
        assert_eq!(watchdog_tick(0, 0, 5, &mut consecutive), 1);
        assert_eq!(watchdog_tick(0, 0, 1, &mut consecutive), 2);
        assert_eq!(watchdog_tick(0, 0, 1, &mut consecutive), 3);
        assert_eq!(watchdog_tick(0, 0, 2, &mut consecutive), 4);
        // 一个干净周期即清零
        assert_eq!(watchdog_tick(7, 0, 0, &mut consecutive), 0);
        // 通道丢弃/未来时间戳拒绝独立于写失败计数推进（33-F4 拆分口径）
        assert_eq!(watchdog_tick(9, 3, 0, &mut consecutive), 0);
        assert_eq!(watchdog_tick(0, 0, 1, &mut consecutive), 1);
    }

    /// 回归：持久写失败（磁盘满/库锁）时 batch 必须有界、send 端背压必须生效。
    /// 旧实现写失败后仍持续从通道 pop——batch 以事件到达速率无界增长,且通道
    /// 永远填不满,CHANNEL_DROPPED 计数恒 0（与 write_batch 文档声明相反）。
    /// 此处用 DROP TABLE 模拟持久写失败：满批后 writer 停止 pop → 通道被
    /// 生产者填满 → CHANNEL_DROPPED 增加;整个失败窗口内 batch 不超过
    /// batch_size + 通道容量（停止旗标置位后的尾排空上限）。
    #[test]
    fn persistent_write_failure_bounds_batch_and_backpressures_channel() {
        use crate::types::{EventAction, EventType};
        use std::sync::atomic::AtomicBool;
        use std::sync::Arc;
        use std::time::{Duration, Instant};

        // 轻量临时目录（对齐 file_activity.rs 测试的做法,无 tempfile 依赖）
        let dir = std::env::temp_dir().join(format!(
            "kyn-coll-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("t.db");
        let db = Arc::new(Database::open(db_path.to_str().unwrap()).unwrap());
        // 制造持久写失败：events 表不存在 → insert_events_with_agg 整批失败
        db.with_writer(
            |w| {
                w.execute("DROP TABLE events", []).unwrap();
            },
            || panic!("writer unavailable"),
        );

        const BATCH: usize = 4;
        const CHAN: usize = 8;
        let (tx, rx) = bounded::<Event>(CHAN);
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        let total_written = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let dropped_before = CHANNEL_DROPPED.load(Ordering::Relaxed);

        let db2 = db.clone();
        let tw2 = total_written.clone();
        let mut batch = Vec::with_capacity(BATCH);
        let handle = std::thread::spawn(move || {
            writer_loop_inner(
                &rx,
                &db2,
                BATCH,
                Duration::from_millis(200),
                tw2.as_ref(),
                &mut batch,
                &stop2,
            )
        });
        // 生产者持续投递（writer 停止 pop 后通道满,应走 CHANNEL_DROPPED 背压）
        let deadline = Instant::now() + Duration::from_millis(2500);
        while Instant::now() < deadline {
            let _ = send_event(&tx, Event::new(EventAction::Press, EventType::Keyboard));
            std::thread::sleep(Duration::from_millis(10));
        }
        stop.store(true, Ordering::Release);
        let final_len = handle.join().unwrap().unwrap_or(0);
        let dropped = CHANNEL_DROPPED.load(Ordering::Relaxed) - dropped_before;
        let _ = std::fs::remove_dir_all(&dir);

        // 背压确实生效：通道被填满,send 端出现丢弃（旧实现恒 0）
        assert!(dropped > 0, "持久写失败期间通道应填满并触发 send 端丢弃");
        // batch 有界：失败窗口内不超过 batch_size + 通道容量 + 尾排空余量
        //（旧实现以到达速率无界增长,本窗口 ~250 次投递远超该上界）
        assert!(
            final_len <= BATCH + CHAN,
            "写失败保留重试的 batch 应有界: got {final_len}"
        );
        assert_eq!(
            total_written.load(Ordering::Relaxed),
            0,
            "全失败不得虚增写入统计"
        );
    }
}
