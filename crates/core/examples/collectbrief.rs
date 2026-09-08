//! Run the collector for a few seconds against a temp DB (end-to-end smoke).
use std::time::Duration;

fn main() {
    let db = std::env::var("KYNOPTIC_DB").unwrap_or_else(|_| "kyn-e2e.db".into());
    let mut c = kynoptic_core::collector::start_collection(&db);
    std::thread::sleep(Duration::from_secs(10));
    c.shutdown();
    println!("collected 10s into {db}");
}
