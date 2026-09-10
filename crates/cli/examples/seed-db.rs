//! Seed a kynoptic DB directly at the storage layer (end-to-end dashboard demo).
//!
//! Passive: writes rows straight into SQLite via the core schema — no synthetic
//! desktop input of any kind (SendInput 等 injection 手段已被移除，见 CODE_NOTES §10).
use chrono::{Duration, Local, TimeZone, Utc};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let db = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "kyn-seed.db".into());
    let conn = rusqlite::Connection::open(&db)?;
    // 与生产库（采集器 Database::open）同口径：WAL，只读打开时才不因 journal_mode 报错
    kynoptic_core::db::apply_pragmas(&conn)?;
    conn.execute_batch(kynoptic_core::db::SCHEMA)?;
    let _ = kynoptic_core::db::run_migrations(&conn);

    let local_now = Local::now();
    let utc = |hour: u32, min: u32, days_ago: i64| -> String {
        // 本地钟面（days_ago 天前的 hour:min）→ UTC RFC3339
        let d = (local_now - Duration::days(days_ago)).date_naive();
        let naive = d.and_hms_opt(hour, min, 0).unwrap();
        Local
            .from_local_datetime(&naive)
            .earliest()
            .unwrap_or_else(|| Local.from_utc_datetime(&naive))
            .with_timezone(&Utc)
            .to_rfc3339()
    };

    let mut n = 0usize;
    {
        let mut stmt = conn.prepare(
            "INSERT INTO events (timestamp, event_type, event_action, event_data, app_name, window_title, session_id) VALUES (?1,?2,?3,?4,?5,?6,NULL)",
        )?;
        let today = 0i64;
        // 今天的几个本地小时桶：按键/点击/窗口切换，多应用分布
        for (hour, app, keys, clicks, switches) in [
            (9u32, "code", 420, 35, 12),
            (10, "code", 880, 60, 20),
            (11, "chrome", 310, 90, 30),
            (14, "terminal", 640, 25, 15),
        ] {
            let base_min = 5u32;
            for i in 0..keys {
                let min = base_min + (i % 50) as u32;
                stmt.execute(rusqlite::params![
                    utc(hour, min, today),
                    "keyboard",
                    "press",
                    serde_json::json!({"key": "seed"}).to_string(),
                    app,
                    app
                ])?;
                n += 1;
            }
            for i in 0..clicks {
                let min = base_min + (i % 55) as u32;
                stmt.execute(rusqlite::params![
                    utc(hour, min, today),
                    "mouse",
                    "click",
                    serde_json::json!({"x": 1, "y": 2}).to_string(),
                    app,
                    app
                ])?;
                n += 1;
            }
            for i in 0..switches {
                let min = base_min + (i * 4 % 55) as u32;
                stmt.execute(rusqlite::params![
                    utc(hour, min, today),
                    "window",
                    "switch",
                    serde_json::json!({"title": "seed"}).to_string(),
                    app,
                    app
                ])?;
                n += 1;
            }
        }
        // 昨天少量事件（供 7 天异常窗口有历史）
        for i in 0..40 {
            stmt.execute(rusqlite::params![
                utc(10, 5 + i % 50, 1),
                "keyboard",
                "press",
                serde_json::json!({"key": "seed"}).to_string(),
                "code",
                "code"
            ])?;
            n += 1;
        }
    }
    println!("seeded {n} events into {db}");
    Ok(())
}
