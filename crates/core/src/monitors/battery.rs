//! 电池状态监控
//!
//! 使用 GetSystemPowerStatus (kernel32) 读取电池信息。

use crate::types::*;
use serde_json::json;
use std::cell::Cell;
use std::mem::zeroed;
use std::time::Duration;

pub struct BatteryMonitor {
    prev_charge: Cell<u8>,
    prev_status: Cell<u8>,
}

impl Default for BatteryMonitor {
    fn default() -> Self {
        Self {
            prev_charge: Cell::new(255),
            prev_status: Cell::new(255),
        }
    }
}

impl Monitor for BatteryMonitor {
    fn name(&self) -> &str {
        "battery"
    }
    fn interval(&self) -> Duration {
        Duration::from_secs(60)
    }

    fn collect(&self, tx: &crossbeam_channel::Sender<Event>) {
        let status = get_battery_status();

        let prev_charge = self.prev_charge.get();
        let prev_status_val = self.prev_status.get();

        // 首次初始化或无电池
        if prev_charge == 255 {
            self.prev_charge.set(status.charge_pct);
            self.prev_status.set(status.status_code);
            if status.battery_flag != 0x80 {
                // 不是 "无电池"，发送初始快照
                let event = Event::new(EventAction::BatteryStatus, EventType::System).data(json!({
                    "percent": status.charge_pct as f64,
                    "status_code": status.status_code,
                    "status_text": status.status_text(),
                    "charging": status.ac_online(),
                }));
                let _ = tx.try_send(event);
            }
            return;
        }

        // 检测变化：电量变化 >= 5% 或状态变化
        let charge_delta = (status.charge_pct as i16 - prev_charge as i16).unsigned_abs() as u8;
        if charge_delta >= 5 || status.status_code != prev_status_val {
            self.prev_charge.set(status.charge_pct);
            self.prev_status.set(status.status_code);

            let event = Event::new(EventAction::BatteryStatus, EventType::System).data(json!({
                "percent": status.charge_pct as f64,
                "status_code": status.status_code,
                "status_text": status.status_text(),
                "charging": status.ac_online(),
                "prev_charge": prev_charge,
            }));
            let _ = tx.try_send(event);
        }
    }
}

#[repr(C)]
struct SYSTEM_POWER_STATUS {
    ac_line_status: u8,
    battery_flag: u8,
    battery_life_percent: u8,
    system_status_flag: u8,
    battery_life_time: u32,
    battery_full_life_time: u32,
}

struct BatteryStatus {
    charge_pct: u8,
    status_code: u8,
    battery_flag: u8,
    ac_status: u8,
}

impl BatteryStatus {
    fn status_text(&self) -> &'static str {
        match self.status_code {
            1 => "discharging",
            2 => "ac",
            3 => "fully_charged",
            4 => "low",
            5 => "critical",
            8 => "charging",
            _ => "unknown",
        }
    }
    fn ac_online(&self) -> bool {
        self.ac_status == 1 || self.ac_status == 2
    }
}

fn get_battery_status() -> BatteryStatus {
    unsafe {
        let mut sps: SYSTEM_POWER_STATUS = zeroed();
        GetSystemPowerStatus(&mut sps);

        let charge = if sps.battery_life_percent > 100 {
            100
        } else if sps.battery_life_percent == 255 {
            0
        } else {
            sps.battery_life_percent
        };

        BatteryStatus {
            charge_pct: charge,
            status_code: sps.battery_flag,
            battery_flag: sps.battery_flag,
            ac_status: sps.ac_line_status,
        }
    }
}

extern "system" {
    fn GetSystemPowerStatus(status: *mut SYSTEM_POWER_STATUS) -> i32;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// status_text 的状态码映射契约（前端按此文案展示电量状态）。
    #[test]
    fn status_text_mapping() {
        let mk = |code: u8| BatteryStatus {
            charge_pct: 50,
            status_code: code,
            battery_flag: 0,
            ac_status: 0,
        };
        assert_eq!(mk(1).status_text(), "discharging");
        assert_eq!(mk(2).status_text(), "ac");
        assert_eq!(mk(3).status_text(), "fully_charged");
        assert_eq!(mk(4).status_text(), "low");
        assert_eq!(mk(5).status_text(), "critical");
        assert_eq!(mk(8).status_text(), "charging");
        assert_eq!(mk(0).status_text(), "unknown");
        assert_eq!(mk(255).status_text(), "unknown");
    }

    /// AC 在线判定：1=市电，2=市电+充电中；其余（如 0=离网）为离线。
    #[test]
    fn ac_online_detection() {
        let mk = |ac: u8| BatteryStatus {
            charge_pct: 50,
            status_code: 1,
            battery_flag: 0,
            ac_status: ac,
        };
        assert!(mk(1).ac_online());
        assert!(mk(2).ac_online());
        assert!(!mk(0).ac_online());
        assert!(!mk(255).ac_online());
    }
}
