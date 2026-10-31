//! 输入设备枚举（Raw Input API，非轮询监控）
//!
//! 用 `GetRawInputDeviceList` + `GetRawInputDeviceInfoW` 枚举本机鼠标/键盘，
//! 并按物理设备归并（[`merge_physical_devices`]）：
//! - 条目 = 一台物理设备（不是 Raw Input 的顶层集合）：复合设备/EC 的
//!   多个集合（`&Colnn`/`&MI_nn` 变体）归并为一条，能力取各集合最大值
//! - 鼠标：物理按键数（numberOfButtons）、有无横向滚轮
//! - 键盘：类型/子类型、功能键数、总键数
//! - 全部设备：接口路径中的 VID/PID（供 usb.ids 式厂商解析）
//! - 鼠标可选标注 `is_touchpad`（精确式触控板，经 Pointer API 分类，
//!   Raw Input 本身不可分辨）
//!
//! 隐私口径：只含硬件能力元数据，不含任何输入内容。
//! 由 [`DeviceMonitor`](crate::monitors::device::DeviceMonitor) 在每次快照时调用，
//! 与上次结果相同则返回 None（不为只增不删的事件表制造重复行）。

use serde::Serialize;
use std::collections::HashSet;
use std::mem::{size_of, zeroed};

/// 一台物理输入设备的能力元数据。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InputDeviceInfo {
    /// "mouse" | "keyboard"
    pub kind: String,
    /// 设备接口名（含 VID/PID），如 \\??\USB#VID_391D&PID_1A04#... 或 HID 虚拟路径。
    /// 归并后为物理设备标识：去掉集合标记与集合序号的路径前缀（见 [`physical_device_path`]）
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
    /// 精确式触控板（Pointer API 分类；Raw Input 按鼠标上报键数≈2 不可分辨）。
    /// None = 未能查询（Pointer API 不可用或触控板集合枚举失败）
    pub is_touchpad: Option<bool>,
}

/// 枚举本机输入设备（鼠标+键盘，按物理设备归并）。失败返回空（不 panic，不阻塞采集线程）。
pub fn enumerate() -> Vec<InputDeviceInfo> {
    use windows_sys::Win32::UI::Input::RAWINPUTDEVICELIST;
    use windows_sys::Win32::UI::Input::{
        GetRawInputDeviceList, RIDI_DEVICEINFO, RID_DEVICE_INFO, RIM_TYPEKEYBOARD, RIM_TYPEMOUSE,
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

        // 触控板分类：精确式触控板的 PTP 集合是独立的 RIM_TYPEHID 句柄，
        // 键鼠集合上 GetPointerDevice 会失败，须在全量句柄上查询
        let touchpad_ids = pointer_touchpad_physical_ids(&list);

        let mut out = Vec::new();
        for dev in &list {
            let kind = match dev.dwType {
                RIM_TYPEMOUSE => "mouse",
                RIM_TYPEKEYBOARD => "keyboard",
                _ => continue,
            };

            // 设备接口名（内嵌 VID/PID）
            let name = raw_device_name(dev.hDevice);
            let (vid, pid) = name.as_deref().map(parse_vid_pid).unwrap_or((None, None));

            // 能力信息
            let mut info: RID_DEVICE_INFO = zeroed();
            let mut info_size = size_of::<RID_DEVICE_INFO>() as u32;
            let ok = windows_sys::Win32::UI::Input::GetRawInputDeviceInfoW(
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

            // 触控板标注：Pointer API 查询可用时给出确定值，否则留 None（未知）
            let phys = name
                .as_deref()
                .map(physical_device_path)
                .unwrap_or_default();
            let is_touchpad = if phys.is_empty() || touchpad_ids.is_empty() {
                None
            } else {
                Some(touchpad_ids.contains(&phys))
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
                is_touchpad,
            });
        }
        merge_physical_devices(out)
    }
}

/// 读 Raw Input 设备接口名。
///
/// 修复说明：`GetRawInputDeviceInfoW` 对 RIDI_DEVICENAME 返回的是**字节数**
/// （UTF-16 每字符 2 字节），此前实现按字符数截断，导致 device_path 尾部
/// 携带数百个 NUL。此处按字节换算字符数并截到首个 NUL。
fn raw_device_name(h_device: windows_sys::Win32::Foundation::HANDLE) -> Option<String> {
    use windows_sys::Win32::UI::Input::{GetRawInputDeviceInfoW, RIDI_DEVICENAME};

    let mut buf = [0u16; 512];
    let mut len_bytes = buf.len() as u32;
    let copied = unsafe {
        GetRawInputDeviceInfoW(
            h_device,
            RIDI_DEVICENAME,
            buf.as_mut_ptr() as _,
            &mut len_bytes,
        )
    };
    if copied == u32::MAX {
        return None;
    }
    let chars = (copied as usize / 2).min(buf.len());
    let s = String::from_utf16_lossy(&buf[..chars]);
    let clean = s.split('\0').next().unwrap_or("");
    if clean.is_empty() {
        None
    } else {
        Some(clean.to_string())
    }
}

/// 物理设备标识：从 Raw Input 接口路径剥离集合级后缀，余下前缀。
///
/// 实机路径形态（实测样例）：
/// `\\?\HID#VID_048D&PID_C100&Col02#7&3a985c43&0&0001#{884b96c3-...}`
/// 规则（依次）：
/// 1. 截掉 `#{接口GUID}` 尾巴；
/// 2. 删除 `&MI_nn`（USB 复合接口）/ `&Colnn`（HID 集合）标记（大小写不敏感）；
/// 3. 删除实例尾部末段的 4 位十六进制集合序号——同一物理设备的各集合仅此段
///    不同（实测 GVInput 的 `1&2d595ca7&1&0000/0001/0002` 共享哈希段）；
///    设备哈希段保留：同型号两台物理设备哈希段不同，不会误合并。
///
/// 末段不足 4 位十六进制的形态（如 ACPI 直连 `4&26247e89&0`）保持原样。
pub fn physical_device_path(path: &str) -> String {
    // 1) 截掉 #{guid} 尾巴
    let base = match path.find("#{") {
        Some(i) => &path[..i],
        None => path,
    };
    // 2) 删除集合/接口标记
    let mut stripped = String::with_capacity(base.len());
    let mut i = 0usize;
    while i < base.len() {
        if base[i..].starts_with('&') {
            let rest = &base[i + 1..];
            let lower = rest.to_ascii_lowercase();
            let (skip, consumed) = if let Some(after) = lower.strip_prefix("mi_") {
                (3, hex_digit_len(after))
            } else if let Some(after) = lower.strip_prefix("col") {
                (3, digit_len(after))
            } else {
                (0, 0)
            };
            if consumed > 0 {
                i += 1 + skip + consumed;
                continue;
            }
        }
        let ch = base[i..].chars().next().expect("非空位置必有字符");
        stripped.push(ch);
        i += ch.len_utf8();
    }
    // 3) 实例尾部末段 4 位十六进制集合序号
    if let Some(pos) = stripped.rfind('&') {
        let tail = &stripped[pos + 1..];
        if tail.len() == 4 && tail.bytes().all(|b| b.is_ascii_hexdigit()) {
            return stripped[..pos].to_string();
        }
    }
    stripped
}

fn hex_digit_len(s: &str) -> usize {
    s.bytes().take_while(|b| b.is_ascii_hexdigit()).count()
}

fn digit_len(s: &str) -> usize {
    s.bytes().take_while(|b| b.is_ascii_digit()).count()
}

/// 按物理设备归并（纯函数）：标识 = [`physical_device_path`] 前缀 + kind。
///
/// - 同标识条目合并为一条：能力取各集合最大值，`*_wheel`/`is_touchpad` 按
///   true > false > None 折叠（任一集合确定为是即整台是）
/// - 归并后 `device_path` 为物理标识（路径前缀）
/// - 无路径的条目不参与归并（无法确认与其他条目同一物理设备，逐条保留）
/// - 输出按 (kind, device_path) 排序，保证同一拓扑的快照字节稳定
pub fn merge_physical_devices(devices: Vec<InputDeviceInfo>) -> Vec<InputDeviceInfo> {
    let mut groups: std::collections::HashMap<(String, String), InputDeviceInfo> =
        std::collections::HashMap::new();
    let mut order: Vec<(String, String)> = Vec::new();
    let mut ungrouped = Vec::new();
    for d in devices {
        let phys = d
            .device_path
            .as_deref()
            .filter(|p| !p.is_empty())
            .map(physical_device_path);
        match phys {
            Some(phys) if !phys.is_empty() => {
                let k = (d.kind.clone(), phys.clone());
                match groups.get_mut(&k) {
                    Some(acc) => fold_capabilities(acc, &d),
                    None => {
                        // 代表条目的 device_path 归一为物理标识（不再是首个集合的完整路径）
                        let mut rep = d;
                        rep.device_path = Some(phys);
                        order.push(k.clone());
                        groups.insert(k, rep);
                    }
                }
            }
            _ => ungrouped.push(d),
        }
    }
    let mut out: Vec<InputDeviceInfo> = order
        .into_iter()
        .map(|k| groups.remove(&k).expect("order 与 groups 同步维护"))
        .collect();
    out.extend(ungrouped);
    out.sort_by(|a, b| {
        a.kind
            .cmp(&b.kind)
            .then_with(|| a.device_path.cmp(&b.device_path))
    });
    out
}

/// 把集合条目 `d` 的能力折入同物理设备的累积条目 `acc`。
fn fold_capabilities(acc: &mut InputDeviceInfo, d: &InputDeviceInfo) {
    acc.buttons = opt_max(acc.buttons, d.buttons);
    acc.keys_total = opt_max(acc.keys_total, d.keys_total);
    acc.function_keys = opt_max(acc.function_keys, d.function_keys);
    acc.has_h_wheel = opt_is(acc.has_h_wheel, d.has_h_wheel);
    acc.is_touchpad = opt_is(acc.is_touchpad, d.is_touchpad);
}

fn opt_max(a: Option<u32>, b: Option<u32>) -> Option<u32> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x.max(y)),
        (None, y) => y,
        (x, None) => x,
    }
}

/// Option<bool> 折叠：任一 Some(true) 即 true；否则任一 Some(false) 即 false；
/// 全未知则 None。
fn opt_is(a: Option<bool>, b: Option<bool>) -> Option<bool> {
    match (a, b) {
        (Some(true), _) | (_, Some(true)) => Some(true),
        (Some(false), _) | (_, Some(false)) => Some(false),
        _ => None,
    }
}

/// Pointer API 触控板发现：遍历全量 Raw Input 句柄（含 RIM_TYPEHID 等非键鼠
/// 集合），`GetPointerDevice` 成功且 `pointerDeviceType` 为触控板（4，实测值，
/// windows-sys 0.59 的 `POINTER_DEVICE_TYPE_TOUCH_PAD` 常量）的计入其物理标识。
/// 任何环节失败返回空集（调用方据此把 is_touchpad 留为 None）。
fn pointer_touchpad_physical_ids(
    list: &[windows_sys::Win32::UI::Input::RAWINPUTDEVICELIST],
) -> HashSet<String> {
    /// 与 Win32 POINTER_DEVICE_INFO 同布局（含 windows-sys 0.59 缺失的
    /// devicePath 尾字段——用其绑定会按短结构收数导致越界写）。
    #[repr(C)]
    struct PointerDeviceInfo {
        display_orientation: u32,
        device: isize,
        pointer_device_type: i32,
        monitor: isize,
        starting_cursor_id: u32,
        max_active_contacts: u16,
        product_string: [u16; 520],
        device_path: *mut u16,
    }
    #[link(name = "user32")]
    extern "system" {
        fn GetPointerDevice(device: isize, pointer_device: *mut PointerDeviceInfo) -> i32;
    }
    #[link(name = "kernel32")]
    extern "system" {
        fn LocalFree(hmem: *mut core::ffi::c_void) -> *mut core::ffi::c_void;
    }
    /// POINTER_DEVICE_TYPE_TOUCH_PAD = 4。实测：精确式触控板（MSFT0001）的
    /// PTP 集合返回 4；3 是触摸/数字化仪（实机 VIRTUAL_DIGITIZER 为 3），勿凭
    /// 记忆改成 3。
    const POINTER_DEVICE_TYPE_TOUCH_PAD: i32 = 4;

    let mut ids = HashSet::new();
    for dev in list {
        unsafe {
            let mut info: PointerDeviceInfo = zeroed();
            if GetPointerDevice(dev.hDevice as isize, &mut info) == 0 {
                continue;
            }
            if info.pointer_device_type == POINTER_DEVICE_TYPE_TOUCH_PAD {
                if let Some(name) = raw_device_name(dev.hDevice) {
                    let phys = physical_device_path(&name);
                    if !phys.is_empty() {
                        ids.insert(phys);
                    }
                }
            }
            if !info.device_path.is_null() {
                LocalFree(info.device_path as *mut core::ffi::c_void);
            }
        }
    }
    ids
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
            // is_touchpad 为本轮新增的可选字段；None=未知，不影响本测试的断言
            is_touchpad: None,
        };
        assert!(
            changed_since(std::slice::from_ref(&a), None),
            "首见非空即变化"
        );
        assert!(
            !changed_since(std::slice::from_ref(&a), Some(std::slice::from_ref(&a))),
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

    // ── 物理设备归并（TDD：测试先于实现，钉住契约）────────────────────
    // 夹具路径均为实机实测的 Raw Input 接口路径形态，勿改形状：
    //   \\?\HID#<硬件ID>[&Colnn|&MI_nn]#<实例哈希段>&<4位集合序号>#{接口GUID}
    // 同一物理设备的各集合共享实例哈希段（如 GVInput 的 1&2d595ca7&1），
    // 仅 &Colnn 标记与末段 4 位集合序号不同；同型号两台物理设备哈希段不同。

    fn touchpad_mouse(path: &str, buttons: Option<u32>, touchpad: Option<bool>) -> InputDeviceInfo {
        InputDeviceInfo {
            kind: "mouse".into(),
            device_path: Some(path.into()),
            vid: None,
            pid: None,
            buttons,
            has_h_wheel: Some(false),
            keys_total: None,
            function_keys: None,
            is_touchpad: touchpad,
        }
    }

    fn ite_keyboard(path: &str, keys_total: Option<u32>) -> InputDeviceInfo {
        InputDeviceInfo {
            kind: "keyboard".into(),
            device_path: Some(path.into()),
            vid: Some("048d".into()),
            pid: Some("c100".into()),
            buttons: None,
            has_h_wheel: None,
            keys_total,
            function_keys: Some(12),
            is_touchpad: None,
        }
    }

    #[test]
    fn physical_device_path_strip_rules() {
        // 实测形态：&Col 标记（大小写不敏感）+ 4 位集合序号 + #{guid} 尾巴
        assert_eq!(
            physical_device_path(
                r"\\?\HID#GVInput&Col01#1&2d595ca7&1&0000#{378de44c-56ef-11d1-bc8c-00a0c91405dd}",
            ),
            r"\\?\HID#GVInput#1&2d595ca7&1",
        );
        // &MI_nn 标记（USB 复合接口）
        assert_eq!(
            physical_device_path(
                r"\\?\HID#VID_046D&PID_C52B&MI_00#8&25d6e1b5&0&0000#{4d1e55b2-f16f-11cf-88cb-001111000030}",
            ),
            r"\\?\HID#VID_046D&PID_C52B#8&25d6e1b5&0",
        );
        // 无 4 位集合序号的形态（ACPI 直连）：末段不是 4 位十六进制则不动
        assert_eq!(
            physical_device_path(
                r"\\?\ACPI#FUJ7401#4&26247e89&0#{884b96c3-56ef-11d1-bc8c-00a0c91405dd}",
            ),
            r"\\?\ACPI#FUJ7401#4&26247e89&0",
        );
        // 无实例尾巴的路径（RDP 等虚拟设备）原样返回
        assert_eq!(
            physical_device_path(r"\\?\ROOT#RDPBUS#0000"),
            r"\\?\ROOT#RDPBUS#0000",
        );
    }

    #[test]
    fn physical_merge_merges_sibling_collections() {
        // 症状 1：同一物理设备的多个集合各自成条目（实机 GVInput：2 鼠集合 + 1 键集合）
        let merged = merge_physical_devices(vec![
            touchpad_mouse(
                r"\\?\HID#GVInput&Col01#1&2d595ca7&1&0000#{378de44c-56ef-11d1-bc8c-00a0c91405dd}",
                Some(5),
                None,
            ),
            touchpad_mouse(
                r"\\?\HID#GVInput&Col02#1&2d595ca7&1&0001#{378de44c-56ef-11d1-bc8c-00a0c91405dd}",
                Some(5),
                None,
            ),
            ite_keyboard(
                r"\\?\HID#GVInput&Col03#1&2d595ca7&1&0002#{884b96c3-56ef-11d1-bc8c-00a0c91405dd}",
                Some(110),
            ),
        ]);
        assert_eq!(merged.len(), 2, "3 个集合应归并为 2 台物理设备（鼠+键）");
        let m = merged.iter().find(|d| d.kind == "mouse").unwrap();
        assert_eq!(
            m.device_path.as_deref(),
            Some(r"\\?\HID#GVInput#1&2d595ca7&1"),
            "归并后 device_path 即物理设备标识（去掉集合标记与集合序号的路径前缀）"
        );
        assert_eq!(m.buttons, Some(5));
    }

    #[test]
    fn physical_merge_keeps_same_model_units_distinct() {
        // 不变量：同型号两台物理设备（实例哈希段不同）不得合并——
        // vid/pid 相同不代表同一台物理设备
        let merged = merge_physical_devices(vec![
            touchpad_mouse(
                r"\\?\HID#VID_1234&PID_5678#7&2a6b8c41&0&0000#{378de44c-56ef-11d1-bc8c-00a0c91405dd}",
                Some(3),
                None,
            ),
            touchpad_mouse(
                r"\\?\HID#VID_1234&PID_5678#7&9d4e1f70&0&0000#{378de44c-56ef-11d1-bc8c-00a0c91405dd}",
                Some(3),
                None,
            ),
        ]);
        assert_eq!(merged.len(), 2, "哈希段不同的两台物理设备必须保持独立");
    }

    #[test]
    fn physical_merge_takes_max_capability() {
        // 实机 ITE EC（VID_048D）：主键盘集合键数少，热键集合键数多，
        // 归并后能力应保留各集合最大值而非丢成热键集合的小数字
        let merged = merge_physical_devices(vec![
            ite_keyboard(
                r"\\?\HID#VID_048D&PID_C100&Col02#7&3a985c43&0&0001#{884b96c3-56ef-11d1-bc8c-00a0c91405dd}",
                Some(12),
            ),
            ite_keyboard(
                r"\\?\HID#VID_048D&PID_C100&Col04#7&3a985c43&0&0003#{884b96c3-56ef-11d1-bc8c-00a0c91405dd}",
                Some(488),
            ),
        ]);
        assert_eq!(merged.len(), 1);
        let kb = &merged[0];
        assert_eq!(kb.keys_total, Some(488), "能力取各集合最大值");
        assert_eq!(kb.function_keys, Some(12));
        assert_eq!(kb.vid.as_deref(), Some("048d"));
    }

    #[test]
    fn physical_merge_touchpad_flag_or_merges() {
        // 症状 2：触控板标注为可选字段，任一集合标注为触控板即整台是触控板
        let merged = merge_physical_devices(vec![
            touchpad_mouse(
                r"\\?\HID#MSFT0001&Col01#4&3b114b9d&0&0000#{378de44c-56ef-11d1-bc8c-00a0c91405dd}",
                Some(2),
                None,
            ),
            touchpad_mouse(
                r"\\?\HID#MSFT0001&Col02#4&3b114b9d&0&0001#{378de44c-56ef-11d1-bc8c-00a0c91405dd}",
                Some(2),
                Some(true),
            ),
        ]);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].is_touchpad, Some(true));
    }

    #[test]
    fn enumerate_reports_unique_physical_devices() {
        // 冒烟（真机）：归并后的输出不得再含同 kind 同物理标识的重复条目
        let devs = enumerate();
        let mut ids: Vec<(String, String)> = devs
            .iter()
            .filter_map(|d| {
                d.device_path
                    .as_ref()
                    .map(|p| (d.kind.clone(), physical_device_path(p)))
            })
            .collect();
        let total = ids.len();
        ids.sort();
        ids.dedup();
        assert_eq!(
            ids.len(),
            total,
            "归并输出不应含重复的物理设备条目: {ids:?}"
        );
        assert!(!devs.is_empty(), "本机应有输入设备");
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
