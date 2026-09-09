//! USB 设备监控
//!
//! 通过 SetupDi API 枚举 USB 设备，检测连接/断开。

use crate::types::*;
use serde_json::json;
use std::cell::Cell;
use std::collections::HashSet;
use std::mem::zeroed;
use std::time::Duration;

pub struct UsbDeviceMonitor {
    prev_devices: Cell<Option<HashSet<String>>>,
}

impl Default for UsbDeviceMonitor {
    fn default() -> Self {
        Self {
            prev_devices: Cell::new(None),
        }
    }
}

impl Monitor for UsbDeviceMonitor {
    fn name(&self) -> &str {
        "usb_device"
    }
    fn interval(&self) -> Duration {
        Duration::from_secs(10)
    }

    fn collect(&self, tx: &crossbeam_channel::Sender<Event>) {
        let devices = enumerate_usb_devices();
        let current_ids: HashSet<String> = devices.iter().map(|d| d.instance_id.clone()).collect();

        let prev = self.prev_devices.take();
        match prev {
            None => {
                // 首次快照
                let event = Event::new(EventAction::UsbDevice, EventType::Device).data(json!({
                    "action": "usb_snapshot",
                    "devices": devices.iter().map(|d| json!({
                        "name": d.name,
                        "instance_id": d.instance_id,
                        "category": d.category,
                    })).collect::<Vec<_>>(),
                    "total_devices": devices.len(),
                }));
                let _ = tx.try_send(event);
            }
            Some(prev_set) => {
                let connected: Vec<_> = current_ids.difference(&prev_set).collect();
                let disconnected: Vec<_> = prev_set.difference(&current_ids).collect();

                for id in &connected {
                    if let Some(dev) = devices.iter().find(|d| &d.instance_id == *id) {
                        let event = Event::new(EventAction::UsbDevice, EventType::Device)
                            .data(json!({
                                "action": "usb_connected",
                                "device": { "name": dev.name, "instance_id": dev.instance_id, "category": dev.category },
                                "total_devices": devices.len(),
                            }));
                        let _ = tx.try_send(event);
                    }
                }
                for id in &disconnected {
                    let event = Event::new(EventAction::UsbDevice, EventType::Device).data(json!({
                        "action": "usb_disconnected",
                        "device": { "instance_id": id },
                        "total_devices": devices.len(),
                    }));
                    let _ = tx.try_send(event);
                }
            }
        }
        self.prev_devices.set(Some(current_ids));
    }
}

#[derive(Clone)]
struct UsbDeviceInfo {
    name: String,
    instance_id: String,
    category: String,
}

fn enumerate_usb_devices() -> Vec<UsbDeviceInfo> {
    let mut devices = Vec::new();

    unsafe {
        let dev_info = SetupDiGetClassDevsW(
            std::ptr::null(),
            std::ptr::null(),
            0,
            DIGCF_PRESENT | DIGCF_ALLCLASSES,
        );
        if dev_info == INVALID_HANDLE_VALUE {
            return devices;
        }

        let mut index: u32 = 0;
        loop {
            let mut dev_data: SP_DEVINFO_DATA = zeroed();
            dev_data.cbSize = std::mem::size_of::<SP_DEVINFO_DATA>() as u32;

            if SetupDiEnumDeviceInfo(dev_info, index, &mut dev_data) == 0 {
                break;
            }

            // 获取实例 ID
            let mut buf = [0u16; 256];
            let mut required_size: u32 = 0;
            SetupDiGetDeviceInstanceIdW(
                dev_info,
                &mut dev_data,
                buf.as_mut_ptr(),
                buf.len() as u32,
                &mut required_size,
            );

            let instance_id = String::from_utf16_lossy(
                &buf[..buf.iter().position(|&c| c == 0).unwrap_or(buf.len())],
            );

            // 只关注 USB 设备
            if !instance_id.to_lowercase().starts_with("usb") {
                index += 1;
                continue;
            }

            // 获取设备描述
            let mut desc_buf = [0u16; 256];
            SetupDiGetDeviceRegistryPropertyW(
                dev_info,
                &mut dev_data,
                SPDRP_DEVICEDESC,
                std::ptr::null_mut(),
                desc_buf.as_mut_ptr(),
                desc_buf.len() as u32,
                std::ptr::null_mut(),
            );

            let name = String::from_utf16_lossy(
                &desc_buf[..desc_buf
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(desc_buf.len())],
            );

            let category = classify_usb_device(&instance_id, &name);

            if !name.is_empty() {
                devices.push(UsbDeviceInfo {
                    name,
                    instance_id,
                    category: category.to_string(),
                });
            }

            index += 1;
        }

        SetupDiDestroyDeviceInfoList(dev_info);
    }

    devices
}

fn classify_usb_device(instance_id: &str, name: &str) -> &'static str {
    let lower = name.to_lowercase();
    let id_lower = instance_id.to_lowercase();
    if lower.contains("hub") || id_lower.contains("hub") {
        "hub"
    } else if lower.contains("storage") || lower.contains("disk") || lower.contains("drive") {
        "storage"
    } else if lower.contains("camera") || lower.contains("webcam") {
        "camera"
    } else if lower.contains("keyboard") {
        "keyboard"
    } else if lower.contains("mouse") {
        "mouse"
    } else if lower.contains("audio") || lower.contains("speaker") || lower.contains("headset") {
        "audio"
    } else if lower.contains("network") || lower.contains("ethernet") || lower.contains("wifi") {
        "network_adapter"
    } else if lower.contains("printer") {
        "printer"
    } else if lower.contains("smart card") {
        "smartcard"
    } else {
        "other"
    }
}

const DIGCF_PRESENT: u32 = 0x00000002;
const DIGCF_ALLCLASSES: u32 = 0x00000004;
const SPDRP_DEVICEDESC: u32 = 0x00000000;
const INVALID_HANDLE_VALUE: isize = -1;

#[repr(C)]
#[allow(non_snake_case)]
struct SP_DEVINFO_DATA {
    cbSize: u32,
    ClassGuid: windows_sys::core::GUID,
    DevInst: u32,
    Reserved: usize,
}

extern "system" {
    fn SetupDiGetClassDevsW(
        classguid: *const windows_sys::core::GUID,
        enumerator: *const u16,
        hwndparent: isize,
        flags: u32,
    ) -> isize;
    fn SetupDiEnumDeviceInfo(devinfo: isize, index: u32, devdata: *mut SP_DEVINFO_DATA) -> i32;
    fn SetupDiGetDeviceRegistryPropertyW(
        devinfo: isize,
        devdata: *mut SP_DEVINFO_DATA,
        property: u32,
        propertyregdatatype: *mut u32,
        propertybuffer: *mut u16,
        propertybuffersize: u32,
        requiredsize: *mut u32,
    ) -> i32;
    fn SetupDiGetDeviceInstanceIdW(
        devinfo: isize,
        devdata: *mut SP_DEVINFO_DATA,
        instanceid: *mut u16,
        size: u32,
        requiredsize: *mut u32,
    ) -> i32;
    fn SetupDiDestroyDeviceInfoList(devinfo: isize) -> i32;
}
