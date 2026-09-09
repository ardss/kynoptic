//! 临时诊断：单监控器采集 + 全量日志打印（调查用，可删）
use std::sync::{Arc, Mutex};

struct Logger(Arc<Mutex<Vec<String>>>);
impl log::Log for Logger {
    fn enabled(&self, m: &log::Metadata) -> bool {
        m.level() <= log::Level::Info
    }
    fn log(&self, r: &log::Record) {
        self.0
            .lock()
            .unwrap()
            .push(format!("[{}] {}: {}", r.level(), r.target(), r.args()));
    }
    fn flush(&self) {}
}

fn main() {
    let id = std::env::args().nth(1).unwrap_or_else(|| "driver".into());
    let secs: u64 = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(30);
    let buf = Arc::new(Mutex::new(Vec::new()));
    log::set_boxed_logger(Box::new(Logger(buf.clone()))).ok();
    log::set_max_level(log::LevelFilter::Info);

    let enabled: std::collections::HashSet<String> = [id.clone()].into_iter().collect();
    let settings = kynoptic_core::collector::CollectorSettings {
        write_flush_interval_secs: 1,
        ..Default::default()
    };
    let dbp = std::env::temp_dir().join(format!("probe-dbg-{}-{}.db", id, std::process::id()));
    let mut c = kynoptic_core::collector::start_collection_custom(
        &enabled,
        settings,
        dbp.to_str().unwrap(),
    );
    std::thread::sleep(std::time::Duration::from_secs(secs));
    c.shutdown();
    let conn = c.db.reader();
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))
        .unwrap_or(0);
    println!("\n== events: {n}");
    for l in buf.lock().unwrap().iter() {
        println!("{l}");
    }
    let _ = std::fs::remove_file(&dbp);
}
