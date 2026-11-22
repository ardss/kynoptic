//! UAC（用户账户控制）事件监控
//!
//! 【默认关闭】PS 子进程实现（powershell spawn），待原生 API 重写后再考虑默认启用。
//!
//! 通过 PowerShell 读取 Windows Defender 和安全日志中的 UAC 相关事件。
//!
//! 隐私边界：日志原文（可能含账号名/进程路径）不落库，只存加盐摘要（前
//! 8 hex）+ 长度 + 事件 id/来源/时间（与 notification/clipboard 同粒度）。

use crate::monitors::ps::run_ps;
use crate::types::*;
use serde_json::json;
use std::cell::Cell;
use std::time::Duration;

pub struct UacMonitor {
    last_event_time: Cell<Option<String>>,
}

impl Default for UacMonitor {
    fn default() -> Self {
        Self {
            last_event_time: Cell::new(None),
        }
    }
}

impl Monitor for UacMonitor {
    fn name(&self) -> &str {
        "uac"
    }
    fn interval(&self) -> Duration {
        Duration::from_secs(60)
    }

    fn collect(&self, tx: &crossbeam_channel::Sender<Event>) {
        let script = r#"
$events = @()
$defender = Get-WinEvent -FilterHashtable @{LogName='Microsoft-Windows-Windows Defender/Operational';Id=1123,5007,2004} -MaxEvents 15 -ErrorAction SilentlyContinue
$defender | ForEach-Object {
    $msg = $_.Message
    if ($msg.Length -gt 200) { $msg = $msg.Substring(0, 200) }
    $events += "$($_.TimeCreated.ToUniversalTime().ToString('o'))|$($_.Id)|Defender|$msg"
}
$privs = Get-WinEvent -FilterHashtable @{LogName='Security';Id=4672} -MaxEvents 5 -ErrorAction SilentlyContinue
$privs | ForEach-Object {
    $msg = $_.Message
    if ($msg.Length -gt 200) { $msg = $msg.Substring(0, 200) }
    $events += "$($_.TimeCreated.ToUniversalTime().ToString('o'))|$($_.Id)|Security|$msg"
}
$events
"#;
        let output = match run_ps(script) {
            Some(o) if !o.trim().is_empty() => o,
            _ => return,
        };

        let prev_time = self.last_event_time.take();
        let (rows, latest_time) =
            super::dedup::parse_watermark_lines(&output, 4, prev_time.as_deref(), 10);
        let events: Vec<_> = rows
            .into_iter()
            .map(|p| {
                // 隐私对齐（与 notification/clipboard 同粒度，平台审查 item 10）：
                // Defender/安全日志原文可能含账号名/进程路径——PII 红线。
                // 原文不落库，只存加盐摘要前 8 hex + 字节长度。
                let digest = super::clipboard::salted_digest_hex(p[3].as_bytes());
                json!({
                    "event_id": p[1].parse::<u32>().unwrap_or(0),
                    "time": p[0],
                    "source": p[2],
                    "digest": &digest[..8],
                    "len": p[3].len(),
                })
            })
            .collect();

        self.last_event_time.set(latest_time);

        if !events.is_empty() {
            let event = Event::new(EventAction::UacEvent, EventType::System)
                .data(json!({ "events": events }));
            let _ = tx.try_send(event);
        }
    }
}
