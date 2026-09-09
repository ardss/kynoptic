//! 文件活动监控
//!
//! 简化轮询版：监控 Desktop / Documents / Downloads 目录的变化时间戳。
//! 完整版可使用 ReadDirectoryChangesW，此处先以轮询方式实现。

use crate::types::*;
use serde_json::json;
use std::cell::Cell;
use std::time::Duration;

pub struct FileActivityMonitor {
    // 记录上次各目录的文件数量，用于检测变化
    prev_counts: Cell<Option<DirCounts>>,
}

#[derive(Clone)]
struct DirCounts {
    desktop: u32,
    documents: u32,
    downloads: u32,
}

impl Default for FileActivityMonitor {
    fn default() -> Self {
        Self {
            prev_counts: Cell::new(None),
        }
    }
}

impl Monitor for FileActivityMonitor {
    fn name(&self) -> &str {
        "file_activity"
    }

    fn interval(&self) -> Duration {
        Duration::from_secs(5)
    }

    fn collect(&self, tx: &crossbeam_channel::Sender<Event>) {
        let desktop = count_dir(&get_desktop_path());
        let documents = count_dir(&get_documents_path());
        let downloads = count_dir(&get_downloads_path());

        let current = DirCounts {
            desktop,
            documents,
            downloads,
        };

        let prev = self.prev_counts.take();
        self.prev_counts.set(Some(current.clone()));

        if let Some(prev) = prev {
            let desktop_delta = current.desktop as i64 - prev.desktop as i64;
            let documents_delta = current.documents as i64 - prev.documents as i64;
            let downloads_delta = current.downloads as i64 - prev.downloads as i64;

            // 只有发生变化时才发送事件
            if desktop_delta != 0 || documents_delta != 0 || downloads_delta != 0 {
                let event = Event::new(EventAction::FileActivity, EventType::Device).data(json!({
                    "desktop_count": current.desktop,
                    "desktop_delta": desktop_delta,
                    "documents_count": current.documents,
                    "documents_delta": documents_delta,
                    "downloads_count": current.downloads,
                    "downloads_delta": downloads_delta,
                }));
                let _ = tx.try_send(event);
            }
        }
        // 首次运行不发送事件
    }
}

fn count_dir(path: &str) -> u32 {
    std::fs::read_dir(path)
        .map(|entries| entries.count() as u32)
        .unwrap_or(0)
}

fn get_desktop_path() -> String {
    if let Ok(userprofile) = std::env::var("USERPROFILE") {
        format!("{}\\Desktop", userprofile)
    } else {
        String::from("C:\\Users\\Default\\Desktop")
    }
}

fn get_documents_path() -> String {
    if let Ok(userprofile) = std::env::var("USERPROFILE") {
        format!("{}\\Documents", userprofile)
    } else {
        String::from("C:\\Users\\Default\\Documents")
    }
}

fn get_downloads_path() -> String {
    if let Ok(userprofile) = std::env::var("USERPROFILE") {
        format!("{}\\Downloads", userprofile)
    } else {
        String::from("C:\\Users\\Default\\Downloads")
    }
}
