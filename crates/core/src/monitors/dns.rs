//! DNS 查询监控
//!
//! 【默认关闭】PS 子进程实现（powershell spawn），待原生 API 重写后再考虑默认启用。
//!
//! 通过 PowerShell Get-WinEvent 读取 DNS 客户端事件日志。

use crate::monitors::ps::run_ps;
use crate::types::*;
use serde_json::json;
use std::cell::Cell;
use std::time::Duration;

pub struct DnsMonitor {
    last_event_time: Cell<Option<String>>,
}

impl Default for DnsMonitor {
    fn default() -> Self {
        Self {
            last_event_time: Cell::new(None),
        }
    }
}

impl Monitor for DnsMonitor {
    fn name(&self) -> &str {
        "dns"
    }
    fn interval(&self) -> Duration {
        Duration::from_secs(60)
    }

    fn collect(&self, tx: &crossbeam_channel::Sender<Event>) {
        let script = r#"
Get-WinEvent -LogName 'Microsoft-Windows-DNS-Client/Operational' -MaxEvents 10 -ErrorAction SilentlyContinue |
  ForEach-Object {
    $xml = [xml]$_.ToXml()
    $query = ($xml.Event.EventData.Data | Where-Object { $_.Name -eq 'QueryName' }).'#text'
    $qtype = ($xml.Event.EventData.Data | Where-Object { $_.Name -eq 'QueryType' }).'#text'
    $qstatus = ($xml.Event.EventData.Data | Where-Object { $_.Name -eq 'QueryStatus' }).'#text'
    "$($_.TimeCreated.ToUniversalTime().ToString('o'))|$query|$qtype|$qstatus"
  }
"#;
        let output = match run_ps(script) {
            Some(o) if !o.trim().is_empty() => o,
            _ => return,
        };

        let prev_time = self.last_event_time.take();
        let (rows, latest_time) =
            super::dedup::parse_watermark_lines(&output, 4, prev_time.as_deref(), 10);
        let queries: Vec<_> = rows
            .into_iter()
            .map(|p| {
                json!({
                    "domain": p[1],
                    "query_type": p[2],
                    "status": p[3],
                    "time_created": p[0],
                })
            })
            .collect();

        self.last_event_time.set(latest_time);

        if !queries.is_empty() {
            let event = Event::new(EventAction::DnsQuery, EventType::Network)
                .data(json!({ "queries": queries }));
            let _ = tx.try_send(event);
        }
    }
}
