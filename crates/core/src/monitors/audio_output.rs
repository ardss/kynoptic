//! 音频输出设备监控
//!
//! 【默认关闭】PS 子进程实现（powershell spawn），待原生 API 重写后再考虑默认启用。
//!
//! 通过 PowerShell 检测音频输出设备变化。

use crate::monitors::ps::run_ps;
use crate::types::*;
use serde_json::json;
use std::cell::Cell;
use std::time::Duration;

pub struct AudioOutputMonitor {
    prev_device: Cell<Option<String>>,
}

impl Default for AudioOutputMonitor {
    fn default() -> Self {
        Self {
            prev_device: Cell::new(None),
        }
    }
}

impl Monitor for AudioOutputMonitor {
    fn name(&self) -> &str {
        "audio_output"
    }
    fn interval(&self) -> Duration {
        Duration::from_secs(60)
    }

    fn collect(&self, tx: &crossbeam_channel::Sender<Event>) {
        let script = r#"
Get-CimInstance Win32_PnPEntity -Namespace root/CIMV2 -ErrorAction SilentlyContinue |
  Where-Object { $_.PNPClass -eq 'AudioEndpoint' -and $_.Status -eq 'OK' } |
  ForEach-Object { "$($_.FriendlyName)" }
"#;
        let output = match run_ps(script) {
            Some(o) if !o.trim().is_empty() => o,
            _ => return,
        };

        let mut device_list: Vec<String> = output
            .trim()
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect();

        // 排序确保状态 key 确定性
        device_list.sort();

        let state_key = device_list.join("|");
        let prev = self.prev_device.take();
        if prev.as_ref() == Some(&state_key) {
            self.prev_device.set(prev);
            return;
        }
        self.prev_device.set(Some(state_key));

        let action = if prev.is_none() {
            "initial"
        } else {
            "device_change"
        };
        let event = Event::new(EventAction::AudioOutput, EventType::System).data(json!({
            "action": action,
            "devices": device_list,
        }));
        let _ = tx.try_send(event);
    }
}
