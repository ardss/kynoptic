//! 打印任务监控
//!
//! 【默认关闭】PS 子进程实现（powershell spawn），待原生 API 重写后再考虑默认启用。
//!
//! 通过 PowerShell Get-PrintJob 检测新打印任务。

use crate::monitors::ps::run_ps;
use crate::types::*;
use serde_json::json;
use std::cell::Cell;
use std::collections::HashSet;
use std::time::Duration;

pub struct PrintMonitor {
    seen_jobs: Cell<Option<HashSet<String>>>,
}

impl Default for PrintMonitor {
    fn default() -> Self {
        Self {
            seen_jobs: Cell::new(None),
        }
    }
}

impl Monitor for PrintMonitor {
    fn name(&self) -> &str {
        "print"
    }
    fn interval(&self) -> Duration {
        Duration::from_secs(30)
    }

    fn collect(&self, tx: &crossbeam_channel::Sender<Event>) {
        let script = r#"
Get-PrintJob -ErrorAction SilentlyContinue |
  ForEach-Object {
    "$($_.Id)|$($_.DocumentName)|$($_.PrinterName)|$($_.JobStatus)|$($_.PagesPrinted)|$($_.SubmittedTime.ToUniversalTime().ToString('o'))"
  }
"#;
        let output = match run_ps(script) {
            Some(o) if !o.trim().is_empty() => o,
            _ => return,
        };

        let mut seen = self.seen_jobs.take().unwrap_or_default();

        for line in output.trim().lines() {
            if line.trim().is_empty() {
                continue;
            }
            let parts: Vec<&str> = line.splitn(6, '|').collect();
            if parts.len() >= 6 {
                let job_key = format!("{}|{}", parts[0].trim(), parts[2].trim());
                if seen.insert(job_key.clone()) {
                    let event = Event::new(EventAction::PrintJob, EventType::System).data(json!({
                        "job_id": parts[0].trim().parse::<u32>().unwrap_or(0),
                        "document": parts[1].trim(),
                        "printer": parts[2].trim(),
                        "status": parts[3].trim(),
                        "pages": parts[4].trim().parse::<u32>().unwrap_or(0),
                        "submitted": parts[5].trim(),
                    }));
                    let _ = tx.try_send(event);
                }
            }
        }

        // 防止 set 无限增长
        if seen.len() > 1000 {
            seen.retain(|k| output.contains(k.split('|').next().unwrap_or("")));
        }

        self.seen_jobs.set(Some(seen));
    }
}
