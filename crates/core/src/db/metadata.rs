//! `metadata` KV 表（schema 版本号等元数据）

use rusqlite::params;

use super::Database;

impl Database {
    pub fn get_metadata(&self, key: &str) -> Option<String> {
        let conn = self.reader();
        conn.query_row(
            "SELECT value FROM metadata WHERE key = ?1",
            params![key],
            |r| r.get(0),
        )
        .ok()
    }

    pub fn set_metadata(&self, key: &str, value: &str) {
        self.with_writer(
            |conn| {
                let _ = conn.execute(
                    "INSERT INTO metadata (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = ?2",
                    params![key, value],
                );
            },
            || {},
        );
    }
}
