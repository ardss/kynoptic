//! IME（输入法）变化监控
//!
//! 【默认关闭】PS 子进程实现（powershell spawn），待原生 API 重写后再考虑默认启用。
//!
//! 通过 PowerShell 读取 TextServicesFramework 事件日志。

use crate::monitors::ps::run_ps;
use crate::types::*;
use serde_json::json;
use std::cell::Cell;
use std::time::Duration;

pub struct ImeMonitor {
    last_event_id: Cell<u64>,
}

impl Default for ImeMonitor {
    fn default() -> Self {
        Self {
            last_event_id: Cell::new(0),
        }
    }
}

impl Monitor for ImeMonitor {
    fn name(&self) -> &str {
        "ime"
    }
    fn interval(&self) -> Duration {
        Duration::from_secs(30)
    }

    fn collect(&self, tx: &crossbeam_channel::Sender<Event>) {
        let script = r#"
Get-WinEvent -LogName 'Microsoft-Windows-TextServicesFramework/Operational' -MaxEvents 10 -ErrorAction SilentlyContinue |
  ForEach-Object {
    $msg = $_.Message
    if ($msg.Length -gt 200) { $msg = $msg.Substring(0, 200) }
    "$($_.RecordId)|$($_.TimeCreated.ToUniversalTime().ToString('o'))|$($_.Id)|$msg"
  }
"#;
        let output = match run_ps(script) {
            Some(o) if !o.trim().is_empty() => o,
            _ => return,
        };

        let prev_id = self.last_event_id.get();
        let mut events = Vec::new();
        let mut latest_id = prev_id;

        for line in output.trim().lines() {
            let parts: Vec<&str> = line.splitn(4, '|').collect();
            if parts.len() >= 4 {
                let record_id: u64 = parts[0].trim().parse().unwrap_or(0);
                if record_id <= prev_id {
                    continue;
                }
                if record_id > latest_id {
                    latest_id = record_id;
                }
                events.push(json!({
                    "record_id": record_id,
                    "time": parts[1].trim(),
                    "event_id": parts[2].trim().parse::<u32>().unwrap_or(0),
                    "message": parts[3].trim(),
                }));
            }
        }

        self.last_event_id.set(latest_id);

        if !events.is_empty() {
            // 发送最新的一条
            if let Some(last) = events.pop() {
                let event = Event::new(EventAction::ImeChange, EventType::System).data(last);
                let _ = tx.try_send(event);
            }
        }
    }
}
