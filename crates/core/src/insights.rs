//! 系统健康告警聚合
//!
//! 把 commands/system.rs::get_insights 里 120 行的"SQL → if-else 算法 → json!"
//! 反模式拆为 4 个独立纯函数，便于 unit-test 与复用。
//!
//! 设计原则：
//! - 每个告警函数输入是已 fetch 的数据（rows / latest data），不直接访问 DB
//! - 阈值集中在 constants 模块，函数体只做"分级 + 文案"
//! - Alert 结构体 derive Serialize，前端 JSON 契约保持原样
//! - 行为 100% 与原 realtime::get_insights 一致（数值/文案/level 都保留）

use serde::Serialize;
use serde_json::Value;

use crate::constants::{
    DEVICE_SNAPSHOT_INTERVAL_HOURS, DISK_ALERT_USED_PCT, DISK_WARN_USED_PCT, THERMAL_ALERT_C,
    THERMAL_WARN_C,
};

/// 告警级别
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AlertLevel {
    Info,
    Warn,
    Alert,
}

/// 告警类别
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AlertKind {
    Disk,
    Thermal,
    Wellbeing,
}

/// 一条告警。前端 JSON 契约：
/// ```json
/// { "level": "alert", "type": "disk", "message": "...",
///   "detail": "...", "drive": "C:", "days_left": 12, "daily_growth_gb": 1.2 }
/// ```
/// `extra` 序列化时展开到顶层（保留磁盘告警的 drive/days_left/daily_growth_gb）。
#[derive(Debug, Clone, Serialize)]
pub struct Alert {
    pub level: AlertLevel,
    #[serde(rename = "type")]
    pub kind: AlertKind,
    pub message: String,
    pub detail: String,
    /// 附加字段：磁盘告警的 drive/days_left/daily_growth_gb,序列化为顶层
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}

/// 磁盘健康告警。
///
/// 输入：`queries::event_history(Device, DeviceSnapshot, 20)` 返回的最新→最旧
/// `Vec<(timestamp, event_data_json_str)>`。算法：取首末两条 snapshot,按 drive 配对
/// 计算日均 free_gb 增长,根据 used_percent 触发 warn/alert 两级。
pub fn disk_alerts(rows: &[(String, String)]) -> Vec<Alert> {
    let mut out = Vec::new();
    if rows.len() < 2 {
        return out;
    }
    let (latest_str, oldest_str) = match (rows.first(), rows.last()) {
        (Some((_, a)), Some((_, b))) => (a, b),
        _ => return out,
    };
    let (Ok(latest), Ok(oldest)) = (
        serde_json::from_str::<Value>(latest_str),
        serde_json::from_str::<Value>(oldest_str),
    ) else {
        return out;
    };
    let (Some(latest_disks), Some(oldest_disks)) = (
        latest.get("disks").and_then(|d| d.as_array()),
        oldest.get("disks").and_then(|d| d.as_array()),
    ) else {
        return out;
    };

    let hours = rows.len() as f64 * DEVICE_SNAPSHOT_INTERVAL_HOURS;
    let days = (hours / 24.0).max(0.1);

    for ld in latest_disks {
        let drive = ld
            .get("drive")
            .and_then(|d| d.as_str())
            .unwrap_or("")
            .to_string();
        if drive.is_empty() {
            continue;
        }
        let free_gb = ld.get("free_gb").and_then(|d| d.as_f64()).unwrap_or(0.0);
        let total_gb = ld.get("total_gb").and_then(|d| d.as_f64()).unwrap_or(0.0);
        let used_pct = ld
            .get("used_percent")
            .and_then(|d| d.as_f64())
            .unwrap_or(0.0);

        let old_free = oldest_disks
            .iter()
            .find(|od| od.get("drive").and_then(|d| d.as_str()) == Some(drive.as_str()))
            .and_then(|od| od.get("free_gb").and_then(|d| d.as_f64()))
            .unwrap_or(free_gb);

        let daily_growth = (old_free - free_gb) / days;

        if used_pct > DISK_ALERT_USED_PCT {
            let days_left = if daily_growth > 0.0 {
                (free_gb / daily_growth).round() as i64
            } else {
                -1
            };
            let mut extra = serde_json::Map::new();
            extra.insert("drive".into(), Value::String(drive.clone()));
            extra.insert("days_left".into(), Value::Number(days_left.into()));
            extra.insert(
                "daily_growth_gb".into(),
                Value::Number(
                    serde_json::Number::from_f64((daily_growth * 10.0).round() / 10.0)
                        .unwrap_or_else(|| 0.into()),
                ),
            );
            out.push(Alert {
                level: AlertLevel::Alert,
                kind: AlertKind::Disk,
                message: format!("{}: 仅剩 {:.0} GB", drive, free_gb),
                detail: if daily_growth > 0.0 && days_left > 0 {
                    format!("日均增长 {:.1} GB，预计 {} 天后满", daily_growth, days_left)
                } else {
                    format!("使用率 {:.0}%", used_pct)
                },
                extra,
            });
        } else if used_pct > DISK_WARN_USED_PCT {
            let msg = format!("{}: {:.0}% 已用", drive, used_pct);
            let mut extra = serde_json::Map::new();
            extra.insert("drive".into(), Value::String(drive));
            out.push(Alert {
                level: AlertLevel::Warn,
                kind: AlertKind::Disk,
                message: msg,
                detail: format!("剩余 {:.0} GB / 共 {:.0} GB", free_gb, total_gb),
                extra,
            });
        }
    }

    out
}

/// 温度告警。输入：最近一次 thermal_snapshot 的 event_data JSON 字符串。
pub fn thermal_alerts(data: Option<&Value>) -> Vec<Alert> {
    let mut out = Vec::new();
    let Some(data) = data else { return out };
    let Some(max_temp) = data.get("max_temp_celsius").and_then(|v| v.as_f64()) else {
        return out;
    };
    if max_temp >= THERMAL_ALERT_C {
        out.push(Alert {
            level: AlertLevel::Alert,
            kind: AlertKind::Thermal,
            message: format!("CPU 温度过高: {:.0}°C", max_temp),
            detail: "建议检查散热或减少高负载任务".to_string(),
            extra: serde_json::Map::new(),
        });
    } else if max_temp >= THERMAL_WARN_C {
        out.push(Alert {
            level: AlertLevel::Warn,
            kind: AlertKind::Thermal,
            message: format!("温度偏高: {:.0}°C", max_temp),
            detail: String::new(),
            extra: serde_json::Map::new(),
        });
    }
    out
}

/// 休息提醒告警。
///
/// 输入：
/// - `last_idle_end`：最后一次 idle_end 事件的 timestamp（None 或空表示从无休息）
/// - `last_idle_start_or_end`：idle_start 或 idle_end 最近一次 timestamp（用于 fallback 计时起点）
///
/// 行为：
/// - 有 idle_end → 不告警（已休息）
/// - 无 idle_end 但有 idle_start → 提示"自 X 起未检测到休息"
/// - 都没有 → 不告警（无 idle 数据）
pub fn rest_alerts(
    last_idle_end: Option<&str>,
    last_idle_start_or_end: Option<&str>,
) -> Vec<Alert> {
    let mut out = Vec::new();
    let has_recent_break = last_idle_end.map(|s| !s.is_empty()).unwrap_or(false);
    if has_recent_break {
        return out;
    }
    if let Some(ts) = last_idle_start_or_end {
        let time_part = if ts.len() >= 16 { &ts[11..16] } else { "??" };
        out.push(Alert {
            level: AlertLevel::Info,
            kind: AlertKind::Wellbeing,
            message: "持续工作中".to_string(),
            detail: format!("自 {} 以来未检测到休息", time_part),
            extra: serde_json::Map::new(),
        });
    }
    out
}

/// 解析一行事件 data JSON 字符串为 Value,失败返回 None（即 `Value::Null`）。
///
/// 命令层批量 `recent_event_data` 后可用 [`crate::json_util::parse_event_data_or_null`] 替代。
pub fn parse_event_data(s: &str) -> Value {
    crate::json_util::parse_event_data_or_null(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(free_gb: f64, total_gb: f64, used_pct: f64, drive: &str) -> String {
        serde_json::json!({
            "disks": [{
                "drive": drive,
                "free_gb": free_gb,
                "total_gb": total_gb,
                "used_percent": used_pct,
            }]
        })
        .to_string()
    }

    #[test]
    fn disk_no_alert_below_warn() {
        let rows = vec![
            (
                "2026-06-17T10:00".into(),
                snapshot(100.0, 200.0, 50.0, "C:"),
            ),
            (
                "2026-06-17T08:00".into(),
                snapshot(105.0, 200.0, 47.5, "C:"),
            ),
        ];
        let alerts = disk_alerts(&rows);
        assert!(alerts.is_empty());
    }

    #[test]
    fn disk_warn_above_70_pct() {
        // 原行为: used_pct > 70.0 触发 warn（严格大于）
        let rows = vec![
            ("2026-06-17T10:00".into(), snapshot(50.0, 200.0, 75.0, "C:")),
            ("2026-06-17T08:00".into(), snapshot(55.0, 200.0, 72.5, "C:")),
        ];
        let alerts = disk_alerts(&rows);
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].level, AlertLevel::Warn);
        assert_eq!(alerts[0].kind, AlertKind::Disk);
        assert_eq!(alerts[0].extra.get("drive").unwrap(), "C:");
    }

    #[test]
    fn disk_alert_at_90_pct_with_growth_estimate() {
        let rows = vec![
            ("2026-06-17T10:00".into(), snapshot(10.0, 200.0, 95.0, "C:")),
            ("2026-06-17T08:00".into(), snapshot(20.0, 200.0, 90.0, "C:")),
        ];
        let alerts = disk_alerts(&rows);
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].level, AlertLevel::Alert);
        // 4h 内 free_gb 减少 10 GB,日均 60 GB,days_left = 10/60*... = 0
        let days_left = alerts[0].extra.get("days_left").unwrap().as_i64().unwrap();
        assert!(days_left >= 0);
        assert!(alerts[0].detail.contains("使用率") || alerts[0].detail.contains("天后满"));
    }

    #[test]
    fn disk_insufficient_rows_returns_empty() {
        let rows = vec![("t".into(), snapshot(100.0, 200.0, 50.0, "C:"))];
        assert!(disk_alerts(&rows).is_empty());
    }

    #[test]
    fn thermal_alert_at_85() {
        let data = serde_json::json!({"max_temp_celsius": 85.0});
        let alerts = thermal_alerts(Some(&data));
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].level, AlertLevel::Alert);
        assert!(alerts[0].message.contains("85"));
    }

    #[test]
    fn thermal_warn_at_72() {
        let data = serde_json::json!({"max_temp_celsius": 72.0});
        let alerts = thermal_alerts(Some(&data));
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].level, AlertLevel::Warn);
    }

    #[test]
    fn thermal_silent_at_60() {
        let data = serde_json::json!({"max_temp_celsius": 60.0});
        assert!(thermal_alerts(Some(&data)).is_empty());
    }

    #[test]
    fn thermal_handles_missing_field() {
        let data = serde_json::json!({"other": 1.0});
        assert!(thermal_alerts(Some(&data)).is_empty());
        assert!(thermal_alerts(None).is_empty());
    }

    #[test]
    fn rest_alert_when_no_idle_end_but_has_idle_start() {
        let alerts = rest_alerts(None, Some("2026-06-17T09:30:00Z"));
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].level, AlertLevel::Info);
        assert_eq!(alerts[0].kind, AlertKind::Wellbeing);
        assert!(alerts[0].detail.contains("09:30"));
    }

    #[test]
    fn rest_silent_when_recent_break() {
        let alerts = rest_alerts(Some("2026-06-17T10:00:00Z"), Some("2026-06-17T10:00:00Z"));
        assert!(alerts.is_empty());
    }

    #[test]
    fn rest_silent_when_nothing() {
        let alerts = rest_alerts(None, None);
        assert!(alerts.is_empty());
    }

    #[test]
    fn parse_event_data_handles_invalid() {
        assert_eq!(parse_event_data("not json"), Value::Null);
        assert_eq!(parse_event_data(""), Value::Null);
        let v = parse_event_data(r#"{"k":1}"#);
        assert_eq!(v["k"], 1);
    }
}
