//! 临时诊断：network 监控器 + 诱导流量，打印逐次 collect 结果（调查用，可删）
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
struct Logger(Arc<Mutex<Vec<String>>>);
impl log::Log for Logger {
    fn enabled(&self, m: &log::Metadata) -> bool {
        m.level() <= log::Level::Debug
    }
    fn log(&self, r: &log::Record) {
        self.0
            .lock()
            .unwrap()
            .push(format!("[{}] {}", r.level(), r.args()));
    }
    fn flush(&self) {}
}
fn main() {
    let buf = Arc::new(Mutex::new(Vec::new()));
    log::set_boxed_logger(Box::new(Logger(buf.clone()))).ok();
    log::set_max_level(log::LevelFilter::Debug);
    let enabled: std::collections::HashSet<String> = ["network".to_string()].into_iter().collect();
    let settings = kynoptic_core::collector::CollectorSettings {
        write_flush_interval_secs: 1,
        ..Default::default()
    };
    let dbp = std::env::temp_dir().join(format!("net-dbg-{}.db", std::process::id()));
    let mut c = kynoptic_core::collector::start_collection_custom(
        &enabled,
        settings,
        dbp.to_str().unwrap(),
    );

    let stop = Arc::new(AtomicBool::new(false));
    let stop2 = stop.clone();
    let ind = std::thread::spawn(move || {
        let sock = std::net::UdpSocket::bind("0.0.0.0:0").unwrap();
        let payload = [0u8; 512];
        while !stop2.load(Ordering::Relaxed) {
            for _ in 0..32 {
                let _ = sock.send_to(&payload, "1.1.1.1:9");
                let _ = sock.send_to(&payload, "8.8.8.8:9");
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    });

    std::thread::sleep(std::time::Duration::from_secs(75));
    stop.store(true, Ordering::Relaxed);
    ind.join().ok();
    c.shutdown();
    let conn = c.db.reader();
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))
        .unwrap_or(0);
    println!("events={n}");
    for l in buf.lock().unwrap().iter() {
        println!("{l}");
    }
    let _ = std::fs::remove_file(&dbp);
}
