//! 显示器配置变化监控
//!
//! 【默认关闭】PS 子进程实现（powershell spawn），待原生 API 重写后再考虑默认启用。
//!
//! 通过 PowerShell Win32_DesktopMonitor 检测显示器增减。

use crate::monitors::ps::run_ps;
use crate::types::*;
use serde_json::json;
use std::cell::Cell;
use std::time::Duration;

pub struct DisplayMonitor {
    prev_displays: Cell<Option<Vec<String>>>,
}

impl Default for DisplayMonitor {
    fn default() -> Self {
        Self {
            prev_displays: Cell::new(None),
        }
    }
}

impl Monitor for DisplayMonitor {
    fn name(&self) -> &str {
        "display"
    }
    fn interval(&self) -> Duration {
        Duration::from_secs(120)
    }

    fn collect(&self, tx: &crossbeam_channel::Sender<Event>) {
        let script = r#"
Get-CimInstance Win32_DesktopMonitor -Namespace root/CIMV2 -ErrorAction SilentlyContinue |
  ForEach-Object {
    "$($_.Name)|$($_.ScreenWidth)|$($_.ScreenHeight)|$($_.Status)"
  }
"#;
        let output = match run_ps(script) {
            Some(o) if !o.trim().is_empty() => o,
            _ => return,
        };

        let mut current = Vec::new();
        let mut names = Vec::new();

        for line in output.trim().lines() {
            let parts: Vec<&str> = line.splitn(4, '|').collect();
            if parts.len() >= 4 {
                let name = parts[0].trim().to_string();
                names.push(name.clone());
                current.push(json!({
                    "name": name,
                    "width": parts[1].trim().parse::<u32>().unwrap_or(0),
                    "height": parts[2].trim().parse::<u32>().unwrap_or(0),
                    "status": parts[3].trim(),
                }));
            }
        }

        let prev = self.prev_displays.take();
        match prev {
            None => {
                // 首次快照
                let event = Event::new(EventAction::DisplayChange, EventType::System).data(json!({
                    "action": "snapshot",
                    "displays": current,
                    "total_displays": current.len(),
                }));
                let _ = tx.try_send(event);
            }
            Some(prev_names) => {
                let added: Vec<_> = names.iter().filter(|n| !prev_names.contains(n)).collect();
                let removed: Vec<_> = prev_names.iter().filter(|n| !names.contains(n)).collect();

                if !added.is_empty() || !removed.is_empty() {
                    let event =
                        Event::new(EventAction::DisplayChange, EventType::System).data(json!({
                            "action": "monitor_change",
                            "added": added,
                            "removed": removed,
                            "total_displays": current.len(),
                        }));
                    let _ = tx.try_send(event);
                }
            }
        }
        self.prev_displays.set(Some(names));
    }
}
