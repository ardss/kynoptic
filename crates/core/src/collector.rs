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

static SHUTDOWN: AtomicBool = AtomicBool::new(false);
static DROPPED_EVENTS: AtomicU64 = AtomicU64::new(0);

pub fn is_shutdown() -> bool {
    SHUTDOWN.load(Ordering::Acquire)
}

pub(crate) fn send_event(tx: &crossbeam_channel::Sender<Event>, event: Event) {
    if tx.try_send(event).is_err() {
        DROPPED_EVENTS.fetch_add(1, Ordering::Relaxed);
    }
}

fn log_dropped_events() {
    let dropped = DROPPED_EVENTS.swap(0, Ordering::Relaxed);
    if dropped > 0 {
        log::warn!("通道已满，丢弃了 {} 个事件", dropped);
    }
}

fn rand_jitter() -> f64 {
    rand::thread_rng().gen_range(-1.0..=1.0)
}

// ─── 采集器设置 ───────────────────────────────────────────────────────────────

/// 输入事件（键盘/鼠标 Hook）的存储粒度。
///
/// **默认 [`InputGranularity::Raw`]：逐事件原样落库（原始数据神圣，不做折叠）**。
/// [`InputGranularity::Minute`] 为 opt-in 的磁盘优化：输入折叠为每分钟每桶一行
/// `input_agg` 计数型事件（见 [`crate::input_agg`]），计数语义（APM/活跃分钟/
/// daily_agg）保持，但按键明细与鼠标坐标不再存储。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InputGranularity {
    /// 逐事件原样落库（默认；行为与 v0.1 一致）
    #[default]
    Raw,
    /// 每分钟每桶一行计数型事件（opt-in 磁盘优化）
    Minute,
}

/// 采集器运行设置（代码内默认值；默认值调整属产品决策，见 CODE_NOTES.md §8）。
#[derive(Debug, Clone, Copy)]
pub struct CollectorSettings {
    /// 输入事件存储粒度（默认 Raw）
    pub input_granularity: InputGranularity,
    /// writer 小批 flush 间隔秒数（默认 30s）。数值是"落库最大延迟"与
    /// "每提交 WAL 页开销主导的磁盘足迹"之间的权衡，仍在校准中。
    pub write_flush_interval_secs: u64,
}

impl Default for CollectorSettings {
    fn default() -> Self {
        Self {
            input_granularity: InputGranularity::default(),
            write_flush_interval_secs: constants::WRITE_FLUSH_INTERVAL_SECS,
        }
    }
}

fn create_monitors() -> Vec<Box<dyn Monitor + Send>> {
    vec![
        Box::new(monitors::window::WindowMonitor::default()),
        Box::new(monitors::idle::IdleMonitor::default()),
        Box::new(monitors::session::SessionMonitor::default()),
        Box::new(monitors::audio::AudioMonitor::default()),
        Box::new(monitors::brightness::BrightnessMonitor::default()),
        Box::new(monitors::process::ProcessMonitor::default()),
        Box::new(monitors::system::SystemMonitor),
        Box::new(monitors::device::DeviceMonitor),
        Box::new(monitors::network::NetworkMonitor::default()),
        Box::new(monitors::battery::BatteryMonitor::default()),
        Box::new(monitors::power_plan::PowerPlanMonitor::default()),
        Box::new(monitors::wifi::WifiMonitor::default()),
    ]
}

fn create_hooks() -> Vec<Box<dyn EventHook>> {
    vec![
        Box::new(monitors::keyboard_hook::KeyboardHook),
        Box::new(monitors::mouse_hook::MouseHook),
    ]
}

fn write_batch(db: &Database, batch: &[Event], total_written: &AtomicUsize) {
    db.insert_events(batch);
    total_written.fetch_add(batch.len(), Ordering::Relaxed);
    // 聚合读缓存增量维护（派生数据；失败仅 log，不影响原始写入）
    db.update_agg(batch);
}

fn writer_loop(
    rx: crossbeam_channel::Receiver<Event>,
    db: Arc<Database>,
    batch_size: usize,
    flush_interval: Duration,
    total_written: Arc<AtomicUsize>,
) {
    use std::panic;

    loop {
        let result = panic::catch_unwind(panic::AssertUnwindSafe(|| {
            writer_loop_inner(&rx, &db, batch_size, flush_interval, &total_written)
        }));
        match result {
            Ok(Some(flush_count)) => {
                log::info!("Writer 正常退出，已 flush {} 条", flush_count);
                return;
            }
            Ok(None) => continue,
            Err(e) => {
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
) -> Option<usize> {
    use crossbeam_channel::{RecvTimeoutError, TryRecvError};
    use std::time::Instant;

    let mut batch: Vec<Event> = Vec::with_capacity(batch_size);
    let mut last_flush = Instant::now();

    loop {
        loop {
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
                        write_batch(db, &batch, total_written);
                    }
                    return Some(n);
                }
            }
        }

        log_dropped_events();

        let now = Instant::now();
        if batch.len() >= batch_size
            || (!batch.is_empty() && now.duration_since(last_flush) >= flush_interval)
        {
            write_batch(db, &batch, total_written);
            batch.clear();
            last_flush = now;
        }

        if batch.is_empty() {
            if SHUTDOWN.load(Ordering::Acquire) {
                // 外层已保证 batch 为空，无需 flush，直接退出
                return Some(0);
            }
            match rx.recv_timeout(flush_interval.min(Duration::from_millis(500))) {
                Ok(event) => batch.push(event),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    let n = batch.len();
                    if n > 0 {
                        write_batch(db, &batch, total_written);
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
            match rx.recv_timeout(until_flush) {
                Ok(event) => batch.push(event),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    let n = batch.len();
                    if n > 0 {
                        write_batch(db, &batch, total_written);
                    }
                    return Some(n);
                }
            }
        }
    }
}

fn run_monitor(m: Box<dyn Monitor + Send>, tx: crossbeam_channel::Sender<Event>) {
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
        if SHUTDOWN.load(Ordering::Acquire) {
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
        while std::time::Instant::now() < deadline {
            if SHUTDOWN.load(Ordering::Acquire) {
                return;
            }
            thread::sleep(Duration::from_millis(200));
        }
    }
}

pub struct Collector {
    pub db: Arc<Database>,
    pub session_id: i64,
    pub total_written: Arc<AtomicUsize>,
    pub writer_handle: Option<thread::JoinHandle<()>>,
    pub hooks: Vec<Box<dyn EventHook>>,
    /// 设置副本：shutdown 时决定是否 flush 未满分钟的部分输入计数。
    settings: CollectorSettings,
}

impl Collector {
    pub fn shutdown(&mut self) {
        SHUTDOWN.store(true, Ordering::Release);

        for h in &self.hooks {
            h.stop();
        }

        // minute 粒度：关停兜底——把当前未满分钟的部分输入计数立即折叠落库，
        // 不等聚合线程的下一次 rollover，保证"最后一分钟"不丢。
        if self.settings.input_granularity == InputGranularity::Minute {
            let events = input_agg::flush_partial(chrono::Local::now());
            if !events.is_empty() {
                write_batch(&self.db, &events, &self.total_written);
                log::info!("关停 flush：{} 条 input_agg 部分分钟事件", events.len());
            }
        }

        if let Some(handle) = self.writer_handle.take() {
            let _ = handle.join();
        }

        let total = self.total_written.load(Ordering::Relaxed) as i64;
        self.db.end_session(self.session_id, total, 0.0);

        log::info!("采集器已停止");
    }
}

/// Drop 兜底：如果 shutdown 之前没被显式调用（例如应用崩溃、强杀、panic），
/// 在析构时仍尝试关闭当前 session，避免幽灵 session 累积。
impl Drop for Collector {
    fn drop(&mut self) {
        if self.writer_handle.is_some() {
            log::warn!(
                "Collector 被 drop 但未调用 shutdown() — 触发兜底关闭 session {}",
                self.session_id
            );
            // 注意：此分支只运行一次（shutdown 会 take writer_handle）
            SHUTDOWN.store(true, Ordering::Release);
            for h in &self.hooks {
                h.stop();
            }
            if self.settings.input_granularity == InputGranularity::Minute {
                let events = input_agg::flush_partial(chrono::Local::now());
                if !events.is_empty() {
                    write_batch(&self.db, &events, &self.total_written);
                }
            }
            if let Some(handle) = self.writer_handle.take() {
                let _ = handle.join();
            }
            let total = self.total_written.load(Ordering::Relaxed) as i64;
            self.db.end_session(self.session_id, total, 0.0);
        }
    }
}

/// 默认设置启动（Raw 粒度 + 30s flush）。需要自定义用 [`start_collection_with`]。
pub fn start_collection(db_path: &str) -> Collector {
    start_collection_with(CollectorSettings::default(), db_path)
}

pub fn start_collection_with(settings: CollectorSettings, db_path: &str) -> Collector {
    env_logger::Builder::from_env("RUST_LOG")
        .filter_level(log::LevelFilter::Info)
        .try_init()
        .ok();

    // 复位全局采集状态，使 Collector 可被重启（如上一个实例已 shutdown）。
    // 此前 SHUTDOWN 一旦置 true 永不复位，第二次 start_collection 的所有线程
    // 会立刻看到 true 而空转退出；DROPPED_EVENTS 也应随新会话清零。
    SHUTDOWN.store(false, Ordering::Release);
    DROPPED_EVENTS.store(0, Ordering::Relaxed);

    let db = Arc::new(Database::open(db_path).expect("数据库初始化失败"));
    // 启动时清扫上次未关闭的 session（崩溃/强杀留的幽灵）
    db.close_ghost_sessions();
    let session_id = db.start_session();
    log::info!("Session {} 已创建", session_id);

    let (tx, rx) = bounded::<Event>(constants::CHANNEL_CAPACITY);
    let total_written = Arc::new(AtomicUsize::new(0));

    let db_w = db.clone();
    let tw = total_written.clone();
    let writer_handle = thread::Builder::new()
        .name("EventWriter".into())
        .spawn(move || {
            writer_loop(
                rx,
                db_w,
                constants::WRITE_BATCH_SIZE,
                Duration::from_secs(settings.write_flush_interval_secs.max(1)),
                tw,
            );
        })
        .expect("Writer 启动失败");

    let monitors = create_monitors();
    let monitor_count = monitors.len();
    log::info!("正在启动 {} 个 Monitor...", monitor_count);

    for m in monitors {
        let tx = tx.clone();
        thread::Builder::new()
            .name(m.name().into())
            .spawn(move || run_monitor(m, tx))
            .expect("Monitor 线程启动失败");
    }

    // 输入粒度：minute 模式下 Hook 回调退化为原子计数，由独立聚合线程每秒
    // drain 并按分钟折叠成 input_agg 事件入队（见 input_agg 模块文档）。
    input_agg::reset();
    let minute_mode = settings.input_granularity == InputGranularity::Minute;
    if minute_mode {
        input_agg::activate();
        let tx_agg = tx.clone();
        thread::Builder::new()
            .name("InputAgg".into())
            .spawn(move || loop {
                thread::sleep(Duration::from_secs(1));
                if SHUTDOWN.load(Ordering::Acquire) {
                    return;
                }
                for e in input_agg::drain(chrono::Local::now()) {
                    send_event(&tx_agg, e);
                }
            })
            .expect("InputAgg 聚合线程启动失败");
    }

    let hooks = create_hooks();
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
    thread::Builder::new()
        .name("Maintenance".into())
        .spawn(move || loop {
            thread::sleep(Duration::from_secs(constants::MAINTENANCE_INTERVAL_SECS));
            if SHUTDOWN.load(Ordering::Acquire) {
                return;
            }
            log::info!("执行定期数据库维护...");
            db_clone.maintenance();
        })
        .expect("维护线程启动失败");

    Collector {
        db,
        session_id,
        total_written,
        writer_handle: Some(writer_handle),
        hooks,
        settings,
    }
}
