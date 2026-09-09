//! Dev soak: enable EVERY registry monitor (test-only) and collect for
//! KYNOPTIC_SOAK_MINS minutes (default 60), printing totals every minute.
use std::sync::atomic::Ordering;
use std::time::Duration;

fn main() {
    let db = std::env::var("KYNOPTIC_DB").unwrap_or_else(|_| "kyn-soak.db".into());
    let mins: u64 = std::env::var("KYNOPTIC_SOAK_MINS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(60);
    let all: std::collections::HashSet<String> = kynoptic_core::registry::MONITOR_REGISTRY
        .iter()
        .map(|s| s.id.to_string())
        .collect();
    println!("enabling {} monitors for {} min into {}", all.len(), mins, db);
    let mut c = kynoptic_core::collector::start_collection_custom(&all, Default::default(), &db);
    for m in 1..=mins {
        std::thread::sleep(Duration::from_secs(60));
        println!(
            "min {}: total_written={}",
            m,
            c.total_written.load(Ordering::Relaxed)
        );
    }
    c.shutdown();
    println!(
        "done: {} events total",
        c.total_written.load(Ordering::Relaxed)
    );
}
