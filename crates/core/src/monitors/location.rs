//! 位置快照监控（简化版）
//!
//! 【默认关闭】PS 子进程实现（powershell spawn），待原生 API 重写后再考虑默认启用。
//!
//! 通过 IP 地理位置服务获取粗略位置。仅在位置变化时发送。

use crate::monitors::ps::run_ps;
use crate::types::*;
use serde_json::json;
use std::cell::Cell;
use std::time::Duration;

/// Haversine 公式计算两点距离（米）
fn haversine(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let r = 6371000.0_f64;
    let dlat = (lat2 - lat1).to_radians();
    let dlon = (lon2 - lon1).to_radians();
    let a = (dlat / 2.0).sin() * (dlat / 2.0).sin()
        + lat1.to_radians().cos()
            * lat2.to_radians().cos()
            * (dlon / 2.0).sin()
            * (dlon / 2.0).sin();
    r * 2.0 * a.sqrt().atan2((1.0 - a).sqrt())
}

pub struct LocationMonitor {
    prev_lat: Cell<f64>,
    prev_lon: Cell<f64>,
    initialized: Cell<bool>,
}

impl Default for LocationMonitor {
    fn default() -> Self {
        Self {
            prev_lat: Cell::new(0.0),
            prev_lon: Cell::new(0.0),
            initialized: Cell::new(false),
        }
    }
}

impl Monitor for LocationMonitor {
    fn name(&self) -> &str {
        "location"
    }
    fn interval(&self) -> Duration {
        Duration::from_secs(300)
    }

    fn collect(&self, tx: &crossbeam_channel::Sender<Event>) {
        let script = r#"
try {
  $r = Invoke-RestMethod -Uri 'https://ipinfo.io/json' -TimeoutSec 10 -ErrorAction Stop
  $loc = $r.loc -split ','
  "$($r.ip)|$($loc[0])|$($loc[1])|$($r.city)|$($r.region)|$($r.country)"
} catch { }
"#;
        let output = match run_ps(script) {
            Some(o) if !o.trim().is_empty() => o,
            _ => return,
        };

        let parts: Vec<&str> = output.trim().split('|').collect();
        if parts.len() < 6 {
            return;
        }

        let lat: f64 = parts[1].trim().parse().unwrap_or(0.0);
        let lon: f64 = parts[2].trim().parse().unwrap_or(0.0);
        if lat == 0.0 && lon == 0.0 {
            return;
        }

        let prev_lat = self.prev_lat.get();
        let prev_lon = self.prev_lon.get();

        // 仅在首次或位置变化 >50m 时发送
        if self.initialized.get() && haversine(prev_lat, prev_lon, lat, lon) < 50.0 {
            return;
        }

        // 模糊精度：保留 3 位小数（约 110m）
        let blur_lat = (lat * 1000.0).round() / 1000.0;
        let blur_lon = (lon * 1000.0).round() / 1000.0;

        self.prev_lat.set(lat);
        self.prev_lon.set(lon);
        self.initialized.set(true);

        let event = Event::new(EventAction::LocationSnapshot, EventType::Location).data(json!({
            "latitude": blur_lat,
            "longitude": blur_lon,
            "source": "ip_address",
            "city": parts[3].trim(),
            "region": parts[4].trim(),
            "country": parts[5].trim(),
        }));
        let _ = tx.try_send(event);
    }
}
