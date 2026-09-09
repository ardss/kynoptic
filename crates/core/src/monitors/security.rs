//! 安全事件监控
//!
//! 【默认关闭】PS 子进程实现（powershell spawn），待原生 API 重写后再考虑默认启用。
//!
//! 通过 PowerShell Get-WinEvent 读取 Windows 安全日志。

use crate::monitors::ps::run_ps;
use crate::types::*;
use serde_json::json;
use std::cell::Cell;
use std::time::Duration;

/// 安全事件 ID → 可读名称
const SECURITY_EVENTS: &[(u32, &str)] = &[
    (4624, "logon_success"),
    (4625, "logon_failed"),
    (4634, "logoff"),
    (4647, "user_initiated_logoff"),
    (4648, "logon_using_explicit_credentials"),
    (4672, "special_privileges"),
    (4720, "account_created"),
    (4722, "account_enabled"),
    (4723, "password_change_attempt"),
    (4725, "account_disabled"),
    (4726, "account_deleted"),
    (4728, "member_added_global_group"),
    (4732, "member_added_local_group"),
    (4756, "member_added_universal_group"),
];

pub struct SecurityMonitor {
    last_event_time: Cell<Option<String>>,
}

impl Default for SecurityMonitor {
    fn default() -> Self {
        Self {
            last_event_time: Cell::new(None),
        }
    }
}

impl Monitor for SecurityMonitor {
    fn name(&self) -> &str {
        "security"
    }
    fn interval(&self) -> Duration {
        Duration::from_secs(60)
    }

    fn collect(&self, tx: &crossbeam_channel::Sender<Event>) {
        let script = r#"
$idlist = '4624','4625','4634','4647','4648','4672','4720','4722','4723','4725','4726','4728','4732','4756'
Get-WinEvent -FilterHashtable @{LogName='Security';Id=$idlist} -MaxEvents 30 -ErrorAction SilentlyContinue |
  ForEach-Object {
    $msg = $_.Message
    if ($msg.Length -gt 200) { $msg = $msg.Substring(0, 200) }
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
                let eid: u32 = parts[1].trim().parse().unwrap_or(0);
                let name = SECURITY_EVENTS
                    .iter()
                    .find(|(id, _)| *id == eid)
                    .map(|(_, n)| *n)
                    .unwrap_or("unknown");
                if events.len() < 10 {
                    events.push(json!({
                        "event_id": eid,
                        "event_name": name,
                        "time": time,
                        "message": parts[2].trim(),
                    }));
                }
            }
        }

        self.last_event_time.set(latest_time);

        if !events.is_empty() {
            let event = Event::new(EventAction::SecurityEvent, EventType::System)
                .data(json!({ "events": events }));
            let _ = tx.try_send(event);
        }
    }
}
