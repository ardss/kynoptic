//! 防火墙事件监控
//!
//! 通过 PowerShell Get-WinEvent 读取防火墙日志。

use crate::monitors::ps::run_ps;
use crate::types::*;
use serde_json::json;
use std::cell::Cell;
use std::time::Duration;

pub struct FirewallMonitor {
    last_event_time: Cell<Option<String>>,
}

impl Default for FirewallMonitor {
    fn default() -> Self {
        Self {
            last_event_time: Cell::new(None),
        }
    }
}

impl Monitor for FirewallMonitor {
    fn name(&self) -> &str {
        "firewall"
    }
    fn interval(&self) -> Duration {
        Duration::from_secs(120)
    }

    fn collect(&self, tx: &crossbeam_channel::Sender<Event>) {
        let script = r#"
Get-WinEvent -LogName 'Microsoft-Windows-Windows Firewall With Advanced Security/Firewall' -MaxEvents 20 -ErrorAction SilentlyContinue |
  ForEach-Object {
    $msg = $_.Message
    if ($msg.Length -gt 300) { $msg = $msg.Substring(0, 300) }
    "$($_.TimeCreated.ToUniversalTime().ToString('o'))|$($_.Id)|$msg"
  }
"#;
        let output = match run_ps(script) {
            Some(o) if !o.trim().is_empty() => o,
            _ => return,
        };

        let prev_time = self.last_event_time.take();
        let (rows, latest_time) =
            super::dedup::parse_watermark_lines(&output, 3, prev_time.as_deref(), 10);
        let events: Vec<_> = rows
            .into_iter()
            .map(|p| {
                json!({
                    "event_id": p[1].parse::<u32>().unwrap_or(0),
                    "time": p[0],
                    "message": p[2],
                })
            })
            .collect();

        self.last_event_time.set(latest_time);

        if !events.is_empty() {
            let event = Event::new(EventAction::FirewallEvent, EventType::Network)
                .data(json!({ "events": events }));
            let _ = tx.try_send(event);
        }
    }
}
