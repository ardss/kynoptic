//! 电源计划监控
//!
//! 通过 powercfg /getactivescheme 检测活动电源计划变化。

use crate::types::*;
use serde_json::json;
use std::cell::Cell;
use std::time::Duration;

pub struct PowerPlanMonitor {
    prev_plan: Cell<Option<String>>,
}

impl Default for PowerPlanMonitor {
    fn default() -> Self {
        Self {
            prev_plan: Cell::new(None),
        }
    }
}

impl Monitor for PowerPlanMonitor {
    fn name(&self) -> &str {
        "power_plan"
    }
    fn interval(&self) -> Duration {
        Duration::from_secs(30)
    }

    fn collect(&self, tx: &crossbeam_channel::Sender<Event>) {
        let plan = get_active_plan();

        let prev = self.prev_plan.take();
        if prev.as_ref() == Some(&plan) {
            self.prev_plan.set(prev);
            return;
        }
        self.prev_plan.set(Some(plan.clone()));

        let event = Event::new(EventAction::PowerPlanChange, EventType::System)
            .data(json!({ "active_plan": plan }));
        let _ = tx.try_send(event);
    }
}

fn get_active_plan() -> String {
    let output = std::process::Command::new("powercfg")
        .args(["/getactivescheme"])
        .output();
    match output {
        Ok(out) => {
            let s = String::from_utf8_lossy(&out.stdout);
            // 输出格式: "Power Scheme GUID: 381b4222-f694-41f0-9685-ff5bb260df2e  (Balanced)"
            // 提取 GUID
            for line in s.lines() {
                if let Some(rest) = line.split("GUID:").nth(1) {
                    let guid = rest.split_whitespace().next().unwrap_or("unknown");
                    return guid.trim().to_string();
                }
            }
            "unknown".to_string()
        }
        Err(_) => "unknown".to_string(),
    }
}
