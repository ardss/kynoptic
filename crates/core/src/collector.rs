use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crossbeam_channel::bounded;
use rand::Rng;

use crate::constants;
use crate::db::Database;
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
                        db.insert_events(&batch);
                        total_written.fetch_add(n, Ordering::Relaxed);
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
            db.insert_events(&batch);
            total_written.fetch_add(batch.len(), Ordering::Relaxed);
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
                        db.insert_events(&batch);
                        total_written.fetch_add(n, Ordering::Relaxed);
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
}

impl Collector {
    pub fn shutdown(&mut self) {
        SHUTDOWN.store(true, Ordering::Release);

        for h in &self.hooks {
            h.stop();
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
            if let Some(handle) = self.writer_handle.take() {
                let _ = handle.join();
            }
            let total = self.total_written.load(Ordering::Relaxed) as i64;
            self.db.end_session(self.session_id, total, 0.0);
        }
    }
}

pub fn start_collection(db_path: &str) -> Collector {
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
                Duration::from_secs(constants::WRITE_FLUSH_INTERVAL_SECS),
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
    }
}
