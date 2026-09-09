//! 蓝牙设备监控
//!
//! 使用 SetupDi API 枚举蓝牙设备，检测连接/断开。

use crate::types::*;
use serde_json::json;
use std::cell::Cell;
use std::collections::HashSet;
use std::time::Duration;

pub struct BluetoothMonitor {
    prev_devices: Cell<Option<HashSet<String>>>,
}

impl Default for BluetoothMonitor {
    fn default() -> Self {
        Self {
            prev_devices: Cell::new(None),
        }
    }
}

impl Monitor for BluetoothMonitor {
    fn name(&self) -> &str {
        "bluetooth"
    }

    fn interval(&self) -> Duration {
        Duration::from_secs(15)
    }

    fn collect(&self, tx: &crossbeam_channel::Sender<Event>) {
        let current = enumerate_bluetooth_devices();

        let prev = self.prev_devices.take();
        self.prev_devices.set(Some(current.clone()));

        match prev {
            None => {
                // 首次运行，只发送快照
                if !current.is_empty() {
                    let event = Event::new(EventAction::BtChange, EventType::Device).data(json!({
                        "action": "snapshot",
                        "devices": current,
                    }));
                    let _ = tx.try_send(event);
                }
            }
            Some(prev_set) => {
                let connected: Vec<&String> = current.difference(&prev_set).collect();
                let disconnected: Vec<&String> = prev_set.difference(&current).collect();

                if !connected.is_empty() || !disconnected.is_empty() {
                    let event = Event::new(EventAction::BtChange, EventType::Device).data(json!({
                        "connected": connected,
                        "disconnected": disconnected,
                    }));
                    let _ = tx.try_send(event);
                }
            }
        }
    }
}

/// GUID_DEVCLASS_BLUETOOTH: e0cbf06c-cd8b-4647-bb8a-90372753335e
const GUID_DEVCLASS_BLUETOOTH: windows_sys::core::GUID =
    windows_sys::core::GUID::from_u128(0xe0cbf06c_cd8b_4647_bb8a_90372753335e);

const INVALID_HANDLE_VALUE: isize = -1;
const DIGCF_PRESENT: u32 = 0x00000002;
const SPDRP_DEVICEDESC: u32 = 0x00000000;

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

    fn SetupDiDestroyDeviceInfoList(devinfo: isize) -> i32;
}

fn enumerate_bluetooth_devices() -> HashSet<String> {
    let mut devices = HashSet::new();

    unsafe {
        let dev_info =
            SetupDiGetClassDevsW(&GUID_DEVCLASS_BLUETOOTH, std::ptr::null(), 0, DIGCF_PRESENT);

        if dev_info == INVALID_HANDLE_VALUE {
            return devices;
        }

        let mut index: u32 = 0;
        loop {
            let mut dev_data: SP_DEVINFO_DATA = std::mem::zeroed();
            dev_data.cbSize = std::mem::size_of::<SP_DEVINFO_DATA>() as u32;

            if SetupDiEnumDeviceInfo(dev_info, index, &mut dev_data) == 0 {
                break;
            }

            let mut buf = [0u16; 256];
            SetupDiGetDeviceRegistryPropertyW(
                dev_info,
                &mut dev_data,
                SPDRP_DEVICEDESC,
                std::ptr::null_mut(),
                buf.as_mut_ptr(),
                buf.len() as u32,
                std::ptr::null_mut(),
            );

            let name = String::from_utf16_lossy(
                &buf[..buf.iter().position(|&c| c == 0).unwrap_or(buf.len())],
            );

            if !name.is_empty() {
                devices.insert(name);
            }

            index += 1;
        }

        SetupDiDestroyDeviceInfoList(dev_info);
    }

    devices
}
