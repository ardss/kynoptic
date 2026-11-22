//! 驱动变化监控
//!
//! 【默认关闭】PS 子进程实现（powershell spawn），待原生 API 重写后再考虑默认启用。
//!
//! 通过 PowerShell 读取系统日志中的驱动相关事件。

use crate::monitors::ps::run_ps;
use crate::types::*;
use serde_json::json;
use std::cell::Cell;
use std::time::Duration;

pub struct DriverMonitor {
    last_event_time: Cell<Option<String>>,
}

impl Default for DriverMonitor {
    fn default() -> Self {
        Self {
            last_event_time: Cell::new(None),
        }
    }
}

impl Monitor for DriverMonitor {
    fn name(&self) -> &str {
        "driver"
    }
    fn interval(&self) -> Duration {
        Duration::from_secs(120)
    }

    fn collect(&self, tx: &crossbeam_channel::Sender<Event>) {
        let script = r#"
Get-WinEvent -FilterHashtable @{
  LogName='System'
  ProviderName='DriverFrameworks-UserMode','Microsoft-Windows-Kernel-PnP'
} -MaxEvents 30 -ErrorAction SilentlyContinue |
  ForEach-Object {
    $msg = $_.Message
    if ($msg.Length -gt 200) { $msg = $msg.Substring(0, 200) }
    "$($_.TimeCreated.ToUniversalTime().ToString('o'))|$($_.Id)|$($_.ProviderName)|$msg"
  }
"#;
        let output = match run_ps(script) {
            Some(o) if !o.trim().is_empty() => o,
            _ => return,
        };

        let prev_time = self.last_event_time.take();
        let (rows, latest_time) =
            super::dedup::parse_watermark_lines(&output, 4, prev_time.as_deref(), 10);
        let events: Vec<_> = rows
            .into_iter()
            .map(|p| {
                json!({
                    "event_id": p[1].parse::<u32>().unwrap_or(0),
                    "time": p[0],
                    "provider": p[2],
                    "message": p[3],
                })
            })
            .collect();

        self.last_event_time.set(latest_time);

        if !events.is_empty() {
            let event = Event::new(EventAction::DriverChange, EventType::Device)
                .data(json!({ "events": events }));
            let _ = tx.try_send(event);
        }
    }
}
