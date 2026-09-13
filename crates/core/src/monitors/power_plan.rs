//! 电源计划监控（原生 PowerGetActiveScheme，零子进程）
//!
//! P0 子进程风暴修复：旧实现每 30s spawn 一次 `powercfg /getactivescheme`
//! （每天 ~2880 次子进程），改用 powrprof.dll 的 PowerGetActiveScheme 直接
//! 读取活动电源计划 GUID。

use crate::types::*;
use serde_json::json;
use std::cell::Cell;
use std::time::Duration;

use windows_sys::Win32::Foundation::ERROR_SUCCESS;
use windows_sys::Win32::System::Power::PowerGetActiveScheme;
use windows_sys::core::GUID;

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
    // 原生路径：PowerGetActiveScheme 返回本地堆分配的 GUID（需 LocalFree，
    // 这里用模块内声明的 LocalFree 释放——kernel32 标准导出）。
    extern "system" {
        fn LocalFree(hMem: *mut core::ffi::c_void) -> *mut core::ffi::c_void;
    }
    unsafe {
        let mut guid: *mut GUID = std::ptr::null_mut();
        if PowerGetActiveScheme(std::ptr::null_mut(), &mut guid) != ERROR_SUCCESS as u32
            || guid.is_null()
        {
            return "unknown".to_string();
        }
        let g = *guid;
        LocalFree(guid.cast());        // 输出与旧 powercfg 解析路径同构：小写连字符 GUID（powercfg 输出即小写）
        format!(
            "{:08x}-{:04x}-{:04x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
            g.data1,
            g.data2,
            g.data3,
            g.data4[0],
            g.data4[1],
            g.data4[2],
            g.data4[3],
            g.data4[4],
            g.data4[5],
            g.data4[6],
            g.data4[7]
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 契约：get_active_plan 返回值必须是 GUID 形态或 "unknown"，
    /// 保证 change 检测的 key 语义稳定。
    #[test]
    fn active_plan_is_guid_or_unknown() {
        let plan = get_active_plan();
        assert!(
            plan == "unknown"
                || (plan.len() == 36 && plan.matches('-').count() == 4),
            "active plan 应为 GUID 字符串或 unknown，实际: {plan}"
        );
    }
}
