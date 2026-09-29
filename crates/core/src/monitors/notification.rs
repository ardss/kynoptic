//! Windows 通知监控
//!
//! 【默认关闭】PS 子进程实现（powershell spawn），待原生 API 重写后再考虑默认启用。
//!
//! 通过 PowerShell Get-WinEvent 读取通知相关事件日志。
//!
//! 隐私边界：只存通知的来源/事件 id/时间与正文的加盐摘要（前 8 hex）+ 长度，
//! 正文原文不落库（与 clipboard「只存摘要」披露粒度对齐）。

use crate::monitors::ps::run_ps;
use crate::types::*;
use serde_json::json;
use std::cell::Cell;
use std::time::Duration;

/// 持久化去重高水位的 metadata 键（平台审查 item 11：跨进程重启保留去重基线）
const DEDUP_KEY: &str = "monitor_notification_last_time";

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

        // 平台审查（item 11）：去重基线取「本进程内存 last_event_time ∪
        // metadata 持久高水位」较新者。首轮（内存为空）时载入持久基线，避免
        // 重启后 Get-WinEvent 日志窗口里的旧条目被整体重放落库（内容完全
        // 重复的行 + retention=0 下永久留存、统计双计）。
        let in_proc = self.last_event_time.take();
        let persisted = super::dedup::load_baseline(DEDUP_KEY);
        let prev_time = match (in_proc.as_ref(), persisted.as_ref()) {
            (Some(a), Some(b)) => Some(if a >= b { a.clone() } else { b.clone() }),
            (Some(a), None) => Some(a.clone()),
            (None, Some(b)) => Some(b.clone()),
            (None, None) => None,
        };
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
                    // 隐私对齐（与 clipboard 同粒度）：通知正文**不落库**，
                    // 只存加盐摘要前 8 hex + 字节长度。正文仅在子进程 stdout
                    // → 本进程内存中转，用于计算摘要后即丢弃。
                    let msg = parts[3].trim();
                    let digest = super::clipboard::salted_digest_hex(msg.as_bytes());
                    events.push(json!({
                        "event_id": parts[1].trim().parse::<u32>().unwrap_or(0),
                        "time": time,
                        "source": parts[2].trim(),
                        "digest": &digest[..8],
                        "len": msg.len(),
                    }));
                }
            }
        }

        self.last_event_time.set(latest_time.clone());

        // 把推进后的去重高水位写回 metadata（仅在较持久基线前进时才写，避免
        // 每 60s 无谓 upsert）；尽力而为，失败不影响本轮采集。
        if let Some(lt) = &latest_time {
            if persisted.as_deref() != Some(lt.as_str()) {
                super::dedup::save_baseline(DEDUP_KEY, lt);
            }
        }

        if !events.is_empty() {
            let event = Event::new(EventAction::Notification, EventType::System)
                .data(json!({ "notifications": events }));
            let _ = tx.try_send(event);
        }
    }
}
