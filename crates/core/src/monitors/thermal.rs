//! 温度传感器监控
//!
//! 【默认关闭】PS 子进程实现（powershell spawn），待原生 API 重写后再考虑默认启用。
//!
//! 通过 PowerShell MSAcpi_ThermalZoneTemperature 读取温度。

use crate::monitors::ps::run_ps;
use crate::types::*;
use serde_json::json;
use std::cell::Cell;
use std::time::Duration;

pub struct ThermalMonitor {
    prev_max_temp: Cell<f64>,
}

impl Default for ThermalMonitor {
    fn default() -> Self {
        Self {
            prev_max_temp: Cell::new(-999.0),
        }
    }
}

impl Monitor for ThermalMonitor {
    fn name(&self) -> &str {
        "thermal"
    }
    fn interval(&self) -> Duration {
        Duration::from_secs(60)
    }

    fn collect(&self, tx: &crossbeam_channel::Sender<Event>) {
        let script = r#"
Get-CimInstance MSAcpi_ThermalZoneTemperature -Namespace root/WMI -ErrorAction SilentlyContinue |
  ForEach-Object {
    "$($_.InstanceName)|$($_.CurrentTemperature)"
  }
"#;
        let output = match run_ps(script) {
            Some(o) if !o.trim().is_empty() => o,
            _ => return,
        };

        let mut zones = Vec::new();
        let mut max_temp = 0.0_f64;

        for line in output.trim().lines() {
            let parts: Vec<&str> = line.splitn(2, '|').collect();
            if parts.len() >= 2 {
                let zone = parts[0].trim();
                let raw: f64 = parts[1].trim().parse().unwrap_or(0.0);
                // WMI 返回 decikelvin，转换为摄氏度
                let temp_c = if raw > 200.0 {
                    (raw / 10.0) - 273.15
                } else {
                    raw
                };
                if temp_c > max_temp {
                    max_temp = temp_c;
                }
                zones.push(json!({
                    "zone": zone,
                    "temp_celsius": (temp_c * 100.0).round() / 100.0,
                }));
            }
        }

        if zones.is_empty() {
            return;
        }

        // 只在温度变化 >= 2°C 时发送事件
        let prev = self.prev_max_temp.get();
        if prev > -900.0 && (max_temp - prev).abs() < 2.0 {
            return;
        }
        self.prev_max_temp.set(max_temp);

        let event = Event::new(EventAction::ThermalSnapshot, EventType::System).data(json!({
            "zones": zones,
            "max_temp_celsius": (max_temp * 100.0).round() / 100.0,
            "warning": max_temp >= 80.0,
        }));
        let _ = tx.try_send(event);
    }
}
