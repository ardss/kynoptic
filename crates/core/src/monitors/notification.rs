//! Windows 通知监控
//!
//! 【默认关闭】PS 子进程实现（powershell spawn），待原生 API 重写后再考虑默认启用。
//!
//! 通过 PowerShell Get-WinEvent 读取通知相关事件日志。

use crate::monitors::ps::run_ps;
use crate::types::*;
use serde_json::json;
use std::cell::Cell;
use std::time::Duration;

pub struct NotificationMonitor {
    last_event_time: Cell<Option<String>>,
}

impl Default for NotificationMonitor {
    fn default() -> Self {
        Self {
            last_event_time: Cell::new(None),
        }
    }
}

impl Monitor for NotificationMonitor {
    fn name(&self) -> &str {
        "notification"
    }
    fn interval(&self) -> Duration {
        Duration::from_secs(60)
    }

    fn collect(&self, tx: &crossbeam_channel::Sender<Event>) {
        let script = r#"
$events = @()
$shell = Get-WinEvent -FilterHashtable @{LogName='Microsoft-Windows-Shell-Core/Operational';Id=9707,9708,28173,28174} -MaxEvents 20 -ErrorAction SilentlyContinue
$shell | ForEach-Object {
    $msg = $_.Message
    if ($msg.Length -gt 200) { $msg = $msg.Substring(0, 200) }
    $events += "$($_.TimeCreated.ToUniversalTime().ToString('o'))|$($_.Id)|Shell|$msg"
}
$app = Get-WinEvent -FilterHashtable @{LogName='Application';ProviderName='Windows.*Toast','ActionCenter'} -MaxEvents 20 -ErrorAction SilentlyContinue
$app | ForEach-Object {
    $msg = $_.Message
    if ($msg.Length -gt 200) { $msg = $msg.Substring(0, 200) }
    $events += "$($_.TimeCreated.ToUniversalTime().ToString('o'))|$($_.Id)|App|$msg"
}
$events
"#;
        let output = match run_ps(script) {
            Some(o) if !o.trim().is_empty() => o,
            _ => return,
        };

        let prev_time = self.last_event_time.take();
        let mut events = Vec::new();
        let mut latest_time = prev_time.clone();

        for line in output.trim().lines() {
            let parts: Vec<&str> = line.splitn(4, '|').collect();
            if parts.len() >= 4 {
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
                        "source": parts[2].trim(),
                        "message": parts[3].trim(),
                    }));
                }
            }
        }

        self.last_event_time.set(latest_time);

        if !events.is_empty() {
            let event = Event::new(EventAction::Notification, EventType::System)
                .data(json!({ "notifications": events }));
            let _ = tx.try_send(event);
        }
    }
}
