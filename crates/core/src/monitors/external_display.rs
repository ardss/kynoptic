//! 外接显示器监控
//!
//! 通过 PowerShell WmiMonitorID 检测外接显示器变化。

use crate::monitors::ps::run_ps;
use crate::types::*;
use serde_json::json;
use std::cell::Cell;
use std::time::Duration;

pub struct ExternalDisplayMonitor {
    prev_monitors: Cell<Option<Vec<String>>>,
}

impl Default for ExternalDisplayMonitor {
    fn default() -> Self {
        Self {
            prev_monitors: Cell::new(None),
        }
    }
}

impl Monitor for ExternalDisplayMonitor {
    fn name(&self) -> &str {
        "external_display"
    }
    fn interval(&self) -> Duration {
        Duration::from_secs(120)
    }

    fn collect(&self, tx: &crossbeam_channel::Sender<Event>) {
        let script = r#"
Get-CimInstance WmiMonitorID -Namespace root/WMI -ErrorAction SilentlyContinue |
  ForEach-Object {
    $name = ($_.UserFriendlyName | Where-Object { $_ -ge 32 -and $_ -le 126 } | ForEach-Object { [char]$_ }) -join ''
    $mfr = ($_.ManufacturerName | Where-Object { $_ -ge 32 -and $_ -le 126 } | ForEach-Object { [char]$_ }) -join ''
    "$mfr|$name"
  }
"#;
        let output = match run_ps(script) {
            Some(o) if !o.trim().is_empty() => o,
            _ => return,
        };

        let mut monitors = Vec::new();
        let mut ids = Vec::new();

        for line in output.trim().lines() {
            let parts: Vec<&str> = line.splitn(2, '|').collect();
            if parts.len() >= 2 {
                let mfr = parts[0].trim().to_string();
                let name = parts[1].trim().to_string();
                if !name.is_empty() {
                    ids.push(format!("{}:{}", mfr, name));
                    monitors.push(json!({
                        "name": name,
                        "manufacturer": mfr,
                    }));
                }
            }
        }

        let prev = self.prev_monitors.take();
        match prev {
            None => {
                let event =
                    Event::new(EventAction::ExternalDisplay, EventType::System).data(json!({
                        "action": "snapshot",
                        "monitors": monitors,
                    }));
                let _ = tx.try_send(event);
            }
            Some(prev_ids) => {
                let added: Vec<_> = ids.iter().filter(|id| !prev_ids.contains(id)).collect();
                let removed: Vec<_> = prev_ids.iter().filter(|id| !ids.contains(id)).collect();

                if !added.is_empty() || !removed.is_empty() {
                    let event =
                        Event::new(EventAction::ExternalDisplay, EventType::System).data(json!({
                            "action": "changed",
                            "added": added,
                            "removed": removed,
                            "monitors": monitors,
                        }));
                    let _ = tx.try_send(event);
                }
            }
        }
        self.prev_monitors.set(Some(ids));
    }
}
