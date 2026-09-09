//! 日历事件监控
//!
//! 【默认关闭】PS 子进程实现（powershell spawn），待原生 API 重写后再考虑默认启用。
//!
//! 通过 PowerShell Outlook COM 对象获取近期日历事件。

use crate::monitors::ps::run_ps;
use crate::types::*;
use serde_json::json;
use std::cell::Cell;
use std::collections::HashSet;
use std::time::Duration;

pub struct CalendarMonitor {
    emitted_keys: Cell<Option<HashSet<String>>>,
}

impl Default for CalendarMonitor {
    fn default() -> Self {
        Self {
            emitted_keys: Cell::new(None),
        }
    }
}

impl Monitor for CalendarMonitor {
    fn name(&self) -> &str {
        "calendar"
    }
    fn interval(&self) -> Duration {
        Duration::from_secs(300)
    }

    fn collect(&self, tx: &crossbeam_channel::Sender<Event>) {
        let script = r#"
try {
  $outlook = New-Object -ComObject Outlook.Application -ErrorAction Stop
  $ns = $outlook.GetNamespace('MAPI')
  $folder = $ns.GetDefaultFolder(9)
  $now = Get-Date
  $window = $now.AddHours(2)
  $items = $folder.Items
  $items.Sort('[Start]')
  $items = $items | Where-Object {
    $_.Start -le $window -and $_.End -ge $now
  }
  $items | ForEach-Object {
    $state = if ($_.Start -gt $now.AddMinutes(5)) { 'upcoming' }
             elseif ($_.End -lt $now.AddMinutes(5)) { 'ending' }
             else { 'ongoing' }
    "$($_.Subject)|$($_.Start.ToUniversalTime().ToString('o'))|$($_.End.ToUniversalTime().ToString('o'))|$($_.Location)|$($_.AllDayEvent)|$state"
  }
} catch { }
"#;
        let output = match run_ps(script) {
            Some(o) if !o.trim().is_empty() => o,
            _ => return,
        };

        let mut emitted = self.emitted_keys.take().unwrap_or_default();
        emitted.clear(); // 每轮清空旧 key

        for line in output.trim().lines() {
            if line.trim().is_empty() {
                continue;
            }
            let parts: Vec<&str> = line.splitn(6, '|').collect();
            if parts.len() >= 6 {
                let subject = parts[0].trim();
                let start = parts[1].trim();
                let state = parts[5].trim();
                let key = format!("{}|{}|{}", subject, start, state);

                if emitted.insert(key.clone()) {
                    let event =
                        Event::new(EventAction::CalendarEvent, EventType::System).data(json!({
                            "subject": subject,
                            "start_time": start,
                            "end_time": parts[2].trim(),
                            "location": parts[3].trim(),
                            "is_all_day": parts[4].trim() == "True",
                            "state": state,
                        }));
                    let _ = tx.try_send(event);
                }
            }
        }

        self.emitted_keys.set(Some(emitted));
    }
}
