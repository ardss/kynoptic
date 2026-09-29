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

/// 持久化去重高水位的 metadata 键（平台审查 item 11：跨进程重启保留已发日历
/// 事件集合，避免同一批事件重放落库）
const DEDUP_KEY: &str = "monitor_calendar_emitted_keys";
/// 持久化去重集合的修剪窗口：start 早于 72h 的 key 仅在其 state 非
/// 'ongoing' 时修剪。PS 查询条件是 `Start ≤ now+2h 且 End ≥ now`——72h 前
/// 开始、仍在进行的多日事件（ongoing）每一拍仍会被查回，其去重键仍在使
/// 用，不得删除（否则重启后 load_baseline 读不到该键，事件被重放落库产
/// 生重复行；retention=0 下永久留存、统计双计）。非 ongoing 的旧 key
/// 剪掉，防持久集合无限增长。
const PRUNE_HOURS: i64 = 72;

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

        // 去重基线：本进程内存 emitted_keys ∪ metadata 持久集合。首轮（内存
        // 为空）载入持久基线——平台审查 item 11：此前「持久化去重」实为进程内
        // Cell，重启后同一批日历事件被整体重放落库（内容完全重复 + retention
        // =0 永久留存）。现在持久基线跨进程保留。（注释与实现对齐：此前注释
        // 自称「持久化去重」但实现只是进程内 HashSet，现真正落 metadata。）
        let in_mem = self.emitted_keys.take();
        let mut emitted = match in_mem {
            Some(s) => s,
            None => super::dedup::load_baseline(DEDUP_KEY).map_or_else(HashSet::new, |s| {
                serde_json::from_str::<HashSet<String>>(&s).unwrap_or_default()
            }),
        };

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

        // 把去重高水位写回 metadata：持久化 emitted_keys（修剪 start 早于
        // 72h 且 state 非 ongoing 的旧 key，防持久集合无限增长；进行中事件
        // 的 key 必须保留，见 prune_old_calendar_keys）。尽力而为：序列化/
        // 写入失败只静默降级为进程内去重，不影响本轮采集。
        persist_emitted_keys(&emitted);
        self.emitted_keys.set(Some(emitted));
    }
}

/// 持久化日历去重集合（先修剪再落库；落库失败静默降级）。
fn persist_emitted_keys(emitted: &HashSet<String>) {
    let mut pruned = emitted.clone();
    prune_old_calendar_keys(&mut pruned);
    if let Ok(s) = serde_json::to_string(&pruned) {
        super::dedup::save_baseline(DEDUP_KEY, &s);
    }
}

/// 修剪 start 早于 PRUNE_HOURS 且 state 非 'ongoing' 的日历去重 key（限制
/// 持久集合增长）。key 形如 `subject|start|state`，start 为第 2 段（RFC3339
/// 'o' 格式，UTC）；解析失败或缺段的 key 一律保留（不丢去重数据，宁多不少）。
///
/// 只豁免 ongoing：PS 查询条件 `Start ≤ now+2h 且 End ≥ now` 允许 start 任意
/// 远在过去——72h 前开始、仍进行中的多日事件每拍仍被查回，其 ongoing key
/// 仍在服役，剪掉它重启后该事件被重放（重复行）。状态迁移对单个事件是单
/// 向的（upcoming→ongoing→ending，随 now 推进），旧 upcoming/ending key
/// 对应事件的当前态必已更晚、key 必已失效（ending 例外窗口：事件结束前
/// ≤5min 内重启会重放该条一次，比改前「重启重放全部窗口事件」窄得多）。
fn prune_old_calendar_keys(keys: &mut HashSet<String>) {
    let now = chrono::Utc::now();
    let cutoff = now - chrono::Duration::hours(PRUNE_HOURS);
    keys.retain(|k| {
        let start_old = match k.split('|').nth(1) {
            Some(s) => match chrono::DateTime::parse_from_rfc3339(s).ok() {
                Some(dt) => dt.with_timezone(&chrono::Utc) < cutoff,
                None => false, // 解析失败：保留
            },
            None => false,
        };
        if !start_old {
            return true;
        }
        match k.split('|').nth(2) {
            Some("ongoing") => true, // 进行中事件仍被查询，key 仍在服役
            Some(_) => false,        // 状态单向迁移，旧 upcoming/ending key 已失效
            None => true,            // 缺 state 段：保留
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn key(subject: &str, start: &str, state: &str) -> String {
        format!("{subject}|{start}|{state}")
    }

    fn hours_ago(h: i64) -> String {
        (chrono::Utc::now() - chrono::Duration::hours(h))
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string()
    }

    /// 回归修复锚定：进行中（ongoing）多日事件的 key 即使 start 早于 72h
    /// 窗口也必须保留——它仍被 `Start ≤ now+2h 且 End ≥ now` 查回，剪掉后
    /// 重启 load_baseline 读不到，事件被重放落库（重复行）。
    #[test]
    fn old_ongoing_key_is_kept() {
        let mut keys = HashSet::new();
        keys.insert(key("跨日事件", &hours_ago(80), "ongoing"));
        prune_old_calendar_keys(&mut keys);
        assert!(
            keys.contains(&key("跨日事件", &hours_ago(80), "ongoing")),
            "start 早于 72h 的 ongoing key 不得被修剪"
        );
    }

    /// 非 ongoing 的旧 key 正常修剪（防集合无限增长的原目的不丢）。
    #[test]
    fn old_non_ongoing_keys_are_pruned() {
        let mut keys = HashSet::new();
        keys.insert(key("旧upcoming", &hours_ago(80), "upcoming"));
        keys.insert(key("旧ending", &hours_ago(80), "ending"));
        keys.insert(key("新ongoing", &hours_ago(1), "ongoing"));
        keys.insert(key("新ending", &hours_ago(1), "ending"));
        prune_old_calendar_keys(&mut keys);
        assert!(!keys.contains(&key("旧upcoming", &hours_ago(80), "upcoming")));
        assert!(!keys.contains(&key("旧ending", &hours_ago(80), "ending")));
        assert!(keys.contains(&key("新ongoing", &hours_ago(1), "ongoing")));
        assert!(keys.contains(&key("新ending", &hours_ago(1), "ending")));
    }

    /// 解析失败/缺段的 key 一律保留（宁多不少，不丢去重数据）。
    #[test]
    fn malformed_keys_are_kept() {
        let mut keys = HashSet::new();
        keys.insert("garbage".to_string()); // 无 '|' 段
        keys.insert(key("坏时间", "not-a-date", "ongoing"));
        keys.insert(format!("{}|{}", "缺段", hours_ago(80))); // 只有 2 段
        prune_old_calendar_keys(&mut keys);
        assert_eq!(keys.len(), 3, "畸形 key 不得被修剪");
    }
}
