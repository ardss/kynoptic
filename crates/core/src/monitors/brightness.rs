//! 亮度监控
//!
//! 尝试通过 dxva2 (GetMonitorBrightness) 获取亮度，
//! 如果不可用则静默跳过。

use crate::types::*;
use serde_json::json;
use std::cell::Cell;
use std::time::Duration;
use windows_sys::Win32::Foundation::*;

pub struct BrightnessMonitor {
    last_brightness: Cell<u32>,
    available: Cell<bool>,
}

impl Default for BrightnessMonitor {
    fn default() -> Self {
        Self {
            last_brightness: Cell::new(u32::MAX),
            available: Cell::new(true),
        }
    }
}

impl Monitor for BrightnessMonitor {
    fn name(&self) -> &str {
        "brightness"
    }

    fn interval(&self) -> Duration {
        Duration::from_secs(30)
    }

    fn collect(&self, tx: &crossbeam_channel::Sender<Event>) {
        // 如果已确认不可用，直接跳过
        if !self.available.get() {
            return;
        }

        let brightness = match get_brightness() {
            Some(b) => b,
            None => {
                self.available.set(false);
                return;
            }
        };

        let prev = self.last_brightness.get();
        if prev != u32::MAX && brightness != prev {
            let event = Event::new(EventAction::BrightnessChange, EventType::System).data(json!({
                "brightness": brightness,
                "old_brightness": prev,
                "new_brightness": brightness,
            }));
            let _ = tx.try_send(event);
        }

        self.last_brightness.set(brightness);
    }
}

/// 通过 dxva2 GetMonitorBrightness 获取亮度
fn get_brightness() -> Option<u32> {
    unsafe {
        // 获取显示器句柄
        let hmonitor = MonitorFromWindow(std::ptr::null_mut(), MONITOR_DEFAULTTOPRIMARY);
        if hmonitor.is_null() {
            return None;
        }

        let mut count: u32 = 0;
        if GetNumberOfPhysicalMonitorsFromHMONITOR(hmonitor, &mut count) == 0 || count == 0 {
            return None;
        }

        // PHYSICAL_MONITOR: HANDLE + wchar_t[128]
        // 每个大小 264 bytes (x64: 8 + 256)
        let buf_size = std::mem::size_of::<PhysicalMonitor>() * count as usize;
        let mut buffer: Vec<u8> = vec![0u8; buf_size];

        if GetPhysicalMonitorsFromHMONITOR(
            hmonitor,
            count,
            buffer.as_mut_ptr() as *mut std::ffi::c_void,
        ) == 0
        {
            return None;
        }

        // 取第一个物理显示器的 handle
        let phys_monitor: PhysicalMonitor =
            std::ptr::read(buffer.as_ptr() as *const PhysicalMonitor);
        let handle = phys_monitor.handle;

        let mut min: u32 = 0;
        let mut current: u32 = 0;
        let mut max: u32 = 0;

        if GetMonitorBrightness(handle, &mut min, &mut current, &mut max) == 0 {
            DestroyPhysicalMonitors(count, buffer.as_mut_ptr() as *mut std::ffi::c_void);
            return None;
        }

        DestroyPhysicalMonitors(count, buffer.as_mut_ptr() as *mut std::ffi::c_void);

        if max == min {
            return Some(current);
        }

        // 归一化到 0-100
        let percent = ((current - min) * 100) / (max - min);
        Some(percent)
    }
}

#[repr(C)]
struct PhysicalMonitor {
    handle: *mut std::ffi::c_void,
    description: [u16; 128],
}

const MONITOR_DEFAULTTOPRIMARY: u32 = 1;

extern "system" {
    // user32
    fn MonitorFromWindow(hwnd: HANDLE, dwflags: u32) -> HANDLE;

    // dxva2.dll
    fn GetNumberOfPhysicalMonitorsFromHMONITOR(hmonitor: HANDLE, count: *mut u32) -> BOOL;
    fn GetPhysicalMonitorsFromHMONITOR(
        hmonitor: HANDLE,
        count: u32,
        monitors: *mut std::ffi::c_void,
    ) -> BOOL;
    fn DestroyPhysicalMonitors(count: u32, monitors: *mut std::ffi::c_void) -> BOOL;
    fn GetMonitorBrightness(
        handle: *mut std::ffi::c_void,
        pdwminimumbrightness: *mut u32,
        pdwcurrentbrightness: *mut u32,
        pdwmaximumbrightness: *mut u32,
    ) -> BOOL;
}
