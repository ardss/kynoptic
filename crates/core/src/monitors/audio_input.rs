//! 音频输入设备变化监控
//!
//! 【默认关闭】PS 子进程实现（powershell spawn），待原生 API 重写后再考虑默认启用。
//!
//! 通过 PowerShell Win32_SoundDevice 检测音频设备变化。

use crate::monitors::ps::run_ps;
use crate::types::*;
use serde_json::json;
use std::cell::Cell;
use std::collections::HashSet;
use std::time::Duration;

pub struct AudioInputMonitor {
    prev_devices: Cell<Option<HashSet<String>>>,
}

impl Default for AudioInputMonitor {
    fn default() -> Self {
        Self {
            prev_devices: Cell::new(None),
        }
    }
}

impl Monitor for AudioInputMonitor {
    fn name(&self) -> &str {
        "audio_input"
    }
    fn interval(&self) -> Duration {
        Duration::from_secs(60)
    }

    fn collect(&self, tx: &crossbeam_channel::Sender<Event>) {
        let script = r#"
Get-CimInstance Win32_SoundDevice -Namespace root/CIMV2 -ErrorAction SilentlyContinue |
  ForEach-Object {
    "$($_.Name)|$($_.Status)"
  }
"#;
        let output = match run_ps(script) {
            Some(o) if !o.trim().is_empty() => o,
            _ => return,
        };

        let mut current = HashSet::new();
        let mut device_list = Vec::new();

        for line in output.trim().lines() {
            if line.trim().is_empty() {
                continue;
            }
            let parts: Vec<&str> = line.splitn(2, '|').collect();
            let name = parts[0].trim().to_string();
            current.insert(name.clone());
            device_list.push(name);
        }

        let prev = self.prev_devices.take();
        match prev {
            None => {
                let event =
                    Event::new(EventAction::AudioInputChange, EventType::System).data(json!({
                        "action": "snapshot",
                        "current_devices": device_list,
                    }));
                let _ = tx.try_send(event);
            }
            Some(prev_set) => {
                let added: Vec<_> = current.difference(&prev_set).collect();
                let removed: Vec<_> = prev_set.difference(&current).collect();

                if !added.is_empty() || !removed.is_empty() {
                    let event =
                        Event::new(EventAction::AudioInputChange, EventType::System).data(json!({
                            "added": added,
                            "removed": removed,
                            "current_devices": device_list,
                        }));
                    let _ = tx.try_send(event);
                }
            }
        }
        self.prev_devices.set(Some(current));
    }
}
