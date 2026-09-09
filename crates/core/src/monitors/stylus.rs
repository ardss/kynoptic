//! 触控笔/手写笔设备监控
//!
//! 【默认关闭】PS 子进程实现（powershell spawn），待原生 API 重写后再考虑默认启用。
//!
//! 通过 PowerShell Get-PnpDevice 检测触控笔连接/断开。

use crate::monitors::ps::run_ps;
use crate::types::*;
use serde_json::json;
use std::cell::Cell;
use std::collections::HashSet;
use std::time::Duration;

pub struct StylusMonitor {
    prev_devices: Cell<Option<HashSet<String>>>,
}

impl Default for StylusMonitor {
    fn default() -> Self {
        Self {
            prev_devices: Cell::new(None),
        }
    }
}

impl Monitor for StylusMonitor {
    fn name(&self) -> &str {
        "stylus"
    }
    fn interval(&self) -> Duration {
        Duration::from_secs(60)
    }

    fn collect(&self, tx: &crossbeam_channel::Sender<Event>) {
        let script = r#"
Get-PnpDevice -Class HID -Status OK -ErrorAction SilentlyContinue |
  Where-Object { $_.FriendlyName -match 'pen|stylus|digitizer|ink' } |
  ForEach-Object {
    "$($_.InstanceId)|$($_.FriendlyName)"
  }
"#;
        let output = match run_ps(script) {
            Some(o) => o,
            _ => return,
        };

        let mut current = HashSet::new();
        let mut devices = Vec::new();

        for line in output.trim().lines() {
            if line.trim().is_empty() {
                continue;
            }
            let parts: Vec<&str> = line.splitn(2, '|').collect();
            if parts.len() >= 2 {
                let id = parts[0].trim().to_string();
                let name = parts[1].trim().to_string();
                current.insert(id.clone());
                devices.push((id, name));
            }
        }

        let prev = self.prev_devices.take();
        match prev {
            None => {
                // 首次不发送事件
            }
            Some(prev_set) => {
                for (id, name) in &devices {
                    if !prev_set.contains(id) {
                        let event =
                            Event::new(EventAction::StylusChange, EventType::Device).data(json!({
                                "action": "connected",
                                "device_name": name,
                                "device_id": id,
                            }));
                        let _ = tx.try_send(event);
                    }
                }
                for id in &prev_set {
                    if !current.contains(id) {
                        let event =
                            Event::new(EventAction::StylusChange, EventType::Device).data(json!({
                                "action": "disconnected",
                                "device_id": id,
                            }));
                        let _ = tx.try_send(event);
                    }
                }
            }
        }
        self.prev_devices.set(Some(current));
    }
}
