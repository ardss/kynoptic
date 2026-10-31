//! 温度传感器监控
//!
//! 【默认关闭】PS 子进程实现（powershell spawn），待原生 API 重写后再考虑默认启用。
//!
//! 通过 PowerShell MSAcpi_ThermalZoneTemperature 读取温度。

use crate::monitors::ps::run_ps;
use crate::types::*;
use serde_json::json;
use std::cell::Cell;
use std::time::{Duration, Instant};

pub struct ThermalMonitor {
    prev_max_temp: Cell<f64>,
    gate: std::cell::RefCell<ThermalGate>,
}

impl Default for ThermalMonitor {
    fn default() -> Self {
        Self {
            prev_max_temp: Cell::new(-999.0),
            gate: std::cell::RefCell::new(ThermalGate::new()),
        }
    }
}

/// 探测退避门（KY-PERF-20261031）：硬件不支持 MSAcpi_ThermalZoneTemperature
/// 的设备上，每 60s 一次的 WMI 查询只是刷 0x8004100C not supported 噪音
/// （实测 15 分钟 14 次）。连续空读驱动指数退避（60→120→240→…上限 900s），
/// 退避到期必再探测一次——可恢复故障不得永久静默；任何有效数据立即复位。
pub struct ThermalGate {
    streak: u32,
    skip_until: Option<Instant>,
}

/// 连续空读次数 → 下次探测前跳过秒数（首空不跳——区分偶发失败与硬性不支持）。
pub fn thermal_backoff_secs(streak: u32) -> u64 {
    match streak {
        0 => 0,
        1 => 60,
        2 => 120,
        n => 240u64.saturating_mul(1 << (n - 3).min(2)).min(900),
    }
}

impl ThermalGate {
    pub const fn new() -> Self {
        ThermalGate {
            streak: 0,
            skip_until: None,
        }
    }
}

impl Default for ThermalGate {
    fn default() -> Self {
        ThermalGate::new()
    }
}

impl ThermalGate {
    pub fn streak(&self) -> u32 {
        self.streak
    }

    /// 本轮是否执行真实 WMI 探测。
    pub fn probe_due(&self, now: Instant) -> bool {
        match self.skip_until {
            Some(t) => now >= t,
            None => true,
        }
    }

    pub fn note_data(&mut self, _now: Instant) {
        self.streak = 0;
        self.skip_until = None;
    }

    pub fn note_empty(&mut self, now: Instant) {
        self.streak = self.streak.saturating_add(1);
        self.skip_until = Some(now + Duration::from_secs(thermal_backoff_secs(self.streak)));
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
        let now = Instant::now();
        if !self.gate.borrow().probe_due(now) {
            return;
        }
        let script = r#"
Get-CimInstance MSAcpi_ThermalZoneTemperature -Namespace root/WMI -ErrorAction SilentlyContinue |
  ForEach-Object {
    "$($_.InstanceName)|$($_.CurrentTemperature)"
  }
"#;
        let output = match run_ps(script) {
            Some(o) if !o.trim().is_empty() => o,
            _ => {
                let mut gate = self.gate.borrow_mut();
                gate.note_empty(Instant::now());
                // 状态可见：进入退避只留一次痕（用户要求可恢复故障不得静默）
                if gate.streak() == 2 {
                    log::info!("温度传感器连续空读（疑似设备不支持），退避探测间隔至多 15 分钟");
                }
                return;
            }
        };
        self.gate.borrow_mut().note_data(Instant::now());

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

/// 退避门单元测试（RED）：空读连续计数驱动指数退避（60s 间隔内跳过 WMI
/// 无效调用），有数据即复位；进退避后必须真的跳过探测（可恢复故障不得
/// 永久静默——退避上限 15 分钟后必然再探测一次）。
#[cfg(test)]
mod gate_tests {
    use super::{thermal_backoff_secs, ThermalGate};
    use std::time::{Duration, Instant};

    fn t(secs: u64) -> Instant {
        Instant::now() - Duration::from_secs(secs)
    }

    #[test]
    fn fresh_gate_probes_and_data_resets() {
        let mut g = ThermalGate::new();
        assert!(g.probe_due(t(0)));
        g.note_data(t(0));
        assert!(g.probe_due(t(0)), "有数据复位后必须立即恢复常规探测");
    }

    #[test]
    fn consecutive_empties_back_off_then_reprobe() {
        let mut g = ThermalGate::new();
        let now = t(0);
        assert!(g.probe_due(now));
        g.note_empty(now);
        assert!(
            !g.probe_due(now + Duration::from_secs(30)),
            "退避窗口内必须跳过"
        );
        assert!(
            g.probe_due(now + Duration::from_secs(120)),
            "退避到期必须再探测"
        );
        g.note_empty(now + Duration::from_secs(120));
        assert!(!g.probe_due(now + Duration::from_secs(200)));
        assert!(
            g.probe_due(now + Duration::from_secs(120 + 240)),
            "指数退避翻倍"
        );
    }

    #[test]
    fn backoff_caps_at_15min_never_permanent_silence() {
        let mut g = ThermalGate::new();
        let mut now = t(0);
        for _ in 0..8 {
            assert!(g.probe_due(now));
            g.note_empty(now);
            now += Duration::from_secs(thermal_backoff_secs(g.streak()));
        }
        let last = thermal_backoff_secs(g.streak());
        assert_eq!(last, 900, "退避上限必须 15 分钟");
    }
}
