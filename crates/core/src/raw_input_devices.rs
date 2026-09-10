//! 输入设备枚举（Raw Input API，非轮询监控）
//!
//! 用 `GetRawInputDeviceList` + `GetRawInputDeviceInfoW` 枚举本机鼠标/键盘：
//! - 鼠标：物理按键数（numberOfButtons）、有无垂直/横向滚轮
//! - 键盘：类型/子类型、功能键数、总键数
//! - 全部设备：接口路径中的 VID/PID（供 usb.ids 式厂商解析）
//!
//! 隐私口径：只含硬件能力元数据，不含任何输入内容。
//! 由 [`DeviceMonitor`](crate::monitors::device::DeviceMonitor) 在每次快照时调用，
//! 与上次结果相同则返回 None（不为只增不删的事件表制造重复行）。

use serde::Serialize;
use std::mem::{size_of, zeroed};

/// 一台输入设备的能力元数据。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InputDeviceInfo {
    /// "mouse" | "keyboard"
    pub kind: String,
    /// 设备接口名（含 VID/PID），如 \\??\USB#VID_391D&PID_1A04#... 或 HID 虚拟路径
    pub device_path: Option<String>,
    /// 从路径解析的 vendor id（十六进制），如 "391d"
    pub vid: Option<String>,
    /// product id
    pub pid: Option<String>,
    /// 鼠标：物理按键数（含侧键；HID 虚拟设备可能为 0）
    pub buttons: Option<u32>,
    /// 鼠标：横向滚轮（倾斜轮/手势区，windows-sys 0.59 无垂直滚轮位）
    pub has_h_wheel: Option<bool>,
    /// 键盘：总键数
    pub keys_total: Option<u32>,
    /// 键盘：功能键数
    pub function_keys: Option<u32>,
}

/// 枚举本机输入设备（鼠标+键盘）。失败返回空（不 panic，不阻塞采集线程）。
pub fn enumerate() -> Vec<InputDeviceInfo> {
    use windows_sys::Win32::UI::Input::RID_DEVICE_INFO;
    use windows_sys::Win32::UI::Input::{
        GetRawInputDeviceInfoW, GetRawInputDeviceList, RAWINPUTDEVICELIST, RIDI_DEVICEINFO,
        RIDI_DEVICENAME, RIM_TYPEKEYBOARD, RIM_TYPEMOUSE,
    };

    unsafe {
        let mut count: u32 = 0;
        // 第一次调用取所需缓冲大小
        if GetRawInputDeviceList(
            std::ptr::null_mut(),
            &mut count,
            size_of::<RAWINPUTDEVICELIST>() as u32,
        ) != 0
        {
            return Vec::new();
        }
        if count == 0 {
            return Vec::new();
        }
        let mut list: Vec<RAWINPUTDEVICELIST> = vec![zeroed(); count as usize];
        let n = GetRawInputDeviceList(
            list.as_mut_ptr(),
            &mut count,
            size_of::<RAWINPUTDEVICELIST>() as u32,
        );
        if n == u32::MAX {
            return Vec::new();
        }
        list.truncate(n as usize);

        let mut out = Vec::new();
        for dev in list {
            let kind = match dev.dwType {
                RIM_TYPEMOUSE => "mouse",
                RIM_TYPEKEYBOARD => "keyboard",
                _ => continue,
            };

            // 设备接口名（内嵌 VID/PID）
            let mut name_buf = [0u16; 512];
            let mut name_len = name_buf.len() as u32;
            let name = if GetRawInputDeviceInfoW(
                dev.hDevice,
                RIDI_DEVICENAME,
                name_buf.as_mut_ptr() as _,
                &mut name_len,
            ) != u32::MAX
                && name_len > 0
            {
                Some(String::from_utf16_lossy(
                    &name_buf[..name_len.min(511) as usize],
                ))
            } else {
                None
            };
            let (vid, pid) = name.as_deref().map(parse_vid_pid).unwrap_or((None, None));

            // 能力信息
            let mut info: RID_DEVICE_INFO = zeroed();
            let mut info_size = size_of::<RID_DEVICE_INFO>() as u32;
            let ok = GetRawInputDeviceInfoW(
                dev.hDevice,
                RIDI_DEVICEINFO,
                &mut info as *mut _ as _,
                &mut info_size,
            ) != u32::MAX;

            let (buttons, has_h_wheel, keys_total, function_keys) = if ok {
                match dev.dwType {
                    RIM_TYPEMOUSE => (
                        Some(info.Anonymous.mouse.dwNumberOfButtons),
                        Some(info.Anonymous.mouse.fHasHorizontalWheel != 0),
                        None,
                        None,
                    ),
                    RIM_TYPEKEYBOARD => (
                        None,
                        None,
                        Some(info.Anonymous.keyboard.dwNumberOfKeysTotal),
                        Some(info.Anonymous.keyboard.dwNumberOfFunctionKeys),
                    ),
                    _ => (None, None, None, None),
                }
            } else {
                (None, None, None, None)
            };

            out.push(InputDeviceInfo {
                kind: kind.to_string(),
                device_path: name,
                vid,
                pid,
                buttons,
                has_h_wheel,
                keys_total,
                function_keys,
            });
        }
        out.sort_by(|a, b| {
            a.kind
                .cmp(&b.kind)
                .then_with(|| a.device_path.cmp(&b.device_path))
        });
        out
    }
}

/// 从 Raw Input 设备路径解析 VID/PID（形如 `...VID_391D&PID_1A04...`）。
fn parse_vid_pid(path: &str) -> (Option<String>, Option<String>) {
    let lower = path.to_ascii_lowercase();
    let vid = lower
        .split("vid_")
        .nth(1)
        .and_then(|rest| rest.get(0..4))
        .map(str::to_string);
    let pid = lower
        .split("pid_")
        .nth(1)
        .and_then(|rest| rest.get(0..4))
        .map(str::to_string);
    (vid, pid)
}

/// 与上次快照比较：相同（设备集合与能力均未变）返回 None。
/// 用于抑制重复事件——只增不删的表里，同一拓扑不该每 30s 写一行。
pub fn changed_since(current: &[InputDeviceInfo], previous: Option<&[InputDeviceInfo]>) -> bool {
    // 枚举失败/无设备时不写行（避免空行刷只增不删的表）
    if current.is_empty() {
        return false;
    }
    match previous {
        None => true,
        Some(prev) => prev != current,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_vid_pid_extracts_hex() {
        let (vid, pid) = parse_vid_pid(
            r"\\??\USB#VID_391D&PID_1A04#5&2fc877c6&0&0000#{884b96c3-56ef-11d1-bc8c-00a0c91405dd}",
        );
        assert_eq!(vid.as_deref(), Some("391d"));
        assert_eq!(pid.as_deref(), Some("1a04"));
    }

    #[test]
    fn parse_vid_pid_missing_fields() {
        let (vid, pid) = parse_vid_pid(r"\\??\ROOT#RDPBUS#0000");
        assert_eq!(vid, None);
        assert_eq!(pid, None);
    }

    #[test]
    fn changed_since_semantics() {
        let a = InputDeviceInfo {
            kind: "mouse".into(),
            device_path: Some(r"\\?\ABC".into()),
            vid: None,
            pid: None,
            buttons: Some(5),
            has_h_wheel: Some(false),
            keys_total: None,
            function_keys: None,
        };
        assert!(changed_since(&[a.clone()], None), "首见非空即变化");
        assert!(
            !changed_since(&[a.clone()], Some(&[a.clone()])),
            "相同拓扑不算变化"
        );
        let mut b = a.clone();
        b.buttons = Some(7);
        assert!(
            changed_since(&[b], Some(std::slice::from_ref(&a))),
            "按键数变化算变化"
        );
        assert!(
            !changed_since(&[], Some(std::slice::from_ref(&a))),
            "拔掉后 current 空不算变化（保守）"
        );
    }

    #[test]
    fn enumerate_runs_on_this_machine() {
        // 冒烟：真机上至少枚举出一个键盘或鼠标；能力字段类型合法
        let devs = enumerate();
        assert!(!devs.is_empty(), "本机应有输入设备");
        assert!(devs.iter().any(|d| d.kind == "keyboard"));
        assert!(devs.iter().any(|d| d.kind == "mouse"));
    }
}
