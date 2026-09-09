//! Windows Update 状态监控
//!
//! 【默认关闭】PS 子进程实现（powershell spawn），待原生 API 重写后再考虑默认启用。
//!
//! 通过 PowerShell COM 对象查询待安装的更新。

use crate::monitors::ps::run_ps;
use crate::types::*;
use serde_json::json;
use std::cell::Cell;
use std::time::Duration;

pub struct WindowsUpdateMonitor {
    prev_pending_count: Cell<i32>,
}

impl Default for WindowsUpdateMonitor {
    fn default() -> Self {
        Self {
            prev_pending_count: Cell::new(-1),
        }
    }
}

impl Monitor for WindowsUpdateMonitor {
    fn name(&self) -> &str {
        "windows_update"
    }
    fn interval(&self) -> Duration {
        Duration::from_secs(3600)
    }

    fn collect(&self, tx: &crossbeam_channel::Sender<Event>) {
        let script = r#"
$session = New-Object -ComObject Microsoft.Update.Session
$searcher = $session.CreateUpdateSearcher()
$result = $searcher.Search('IsInstalled=0')
$pending = $result.Updates.Count
$updates = $result.Updates | Select-Object -First 20 | ForEach-Object {
    $size = if ($_.MaxDownloadSize -gt 0) { [math]::Round($_.MaxDownloadSize / 1MB, 1) } else { 0 }
    "$($_.Title)|$($_.MsrcSeverity)|$size|$($_.Identity.UpdateID)"
}
Write-Output "COUNT:$pending"
$updates
"#;
        let output = match run_ps(script) {
            Some(o) if !o.trim().is_empty() => o,
            _ => return,
        };

        let mut pending_count = 0i32;
        let mut updates = Vec::new();

        for line in output.trim().lines() {
            if let Some(count_str) = line.strip_prefix("COUNT:") {
                pending_count = count_str.trim().parse().unwrap_or(0);
                continue;
            }
            let parts: Vec<&str> = line.splitn(4, '|').collect();
            if parts.len() >= 4 {
                updates.push(json!({
                    "title": parts[0].trim(),
                    "severity": parts[1].trim(),
                    "size_mb": parts[2].trim().parse::<f64>().unwrap_or(0.0),
                    "id": parts[3].trim(),
                }));
            }
        }

        let prev = self.prev_pending_count.get();
        if prev >= 0 && pending_count == prev {
            return;
        }
        self.prev_pending_count.set(pending_count);

        let event = Event::new(EventAction::UpdateStatus, EventType::System).data(json!({
            "pending_count": pending_count,
            "updates": updates,
        }));
        let _ = tx.try_send(event);
    }
}
