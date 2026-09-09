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
        let mut events = Vec::new();
        let mut latest_time = prev_time.clone();

        for line in output.trim().lines() {
            let parts: Vec<&str> = line.splitn(3, '|').collect();
            if parts.len() >= 3 {
                let time = parts[0].trim().to_string();
                if let Some(ref pt) = prev_time {
                    if time <= *pt {
                        continue;
                    }
                }
                if latest_time.as_ref().is_none_or(|lt| time > *lt) {
                    latest_time = Some(time.clone());
                }
                if events.len() < 10 {
                    events.push(json!({
                        "event_id": parts[1].trim().parse::<u32>().unwrap_or(0),
                        "time": time,
                        "message": parts[2].trim(),
                    }));
                }
            }
        }

        self.last_event_time.set(latest_time);

        if !events.is_empty() {
            let event = Event::new(EventAction::FirewallEvent, EventType::Network)
                .data(json!({ "events": events }));
            let _ = tx.try_send(event);
        }
    }
}
