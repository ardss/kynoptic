//! IME（输入法）变化监控
//!
//! 轮询注册表键盘布局指纹（原生 windows-sys，无 PS 子进程）：
//! - HKCU\Keyboard Layout\Preload —— 当前用户会话启用的键盘布局列表，
//!   每台 Windows 机器都存在，任何语言下切换输入法/布局都会变化。
//! - HKCU\Control Panel\International\User Profile 的 InputMethodOverride
//!   值（用户设置的系统级输入法覆盖）。
//!
//! 指纹 = Preload 各 REG_SZ 值排序拼接 + override 值。指纹变化才产出
//! ime_change 事件，事件只含布局 ID 元数据，不含按键内容。

use crate::types::*;
use serde_json::json;
use std::cell::Cell;
use std::time::Duration;
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::System::Registry::*;

pub struct ImeMonitor {
    last_fingerprint: Cell<Option<String>>,
}

impl Default for ImeMonitor {
    fn default() -> Self {
        Self {
            last_fingerprint: Cell::new(None),
        }
    }
}

impl Monitor for ImeMonitor {
    fn name(&self) -> &str {
        "ime"
    }
    fn interval(&self) -> Duration {
        Duration::from_secs(5)
    }

    fn collect(&self, tx: &crossbeam_channel::Sender<Event>) {
        let layouts = enum_preload_layouts();
        let override_ = query_input_method_override();

        let prev = self.last_fingerprint.take();
        let (fingerprint, event) = compute_change(prev.as_deref(), &layouts, &override_);
        self.last_fingerprint.set(Some(fingerprint));

        if let Some(event) = event {
            let _ = tx.try_send(event);
        }
    }
}

/// 计算指纹并判断是否产出 ime_change 事件。
/// 返回 (新指纹, 可选事件)。prev 为 None 时只建基线，不发事件。
fn compute_change(
    prev: Option<&str>,
    layouts: &[String],
    override_: &Option<String>,
) -> (String, Option<Event>) {
    let mut sorted: Vec<&str> = layouts.iter().map(|s| s.as_str()).collect();
    sorted.sort();
    let fingerprint = format!("{}|{}", sorted.join(","), override_.clone().unwrap_or_default());

    let event = match prev {
        None => None,
        Some(p) if p == fingerprint => None,
        Some(_) => Some(
            Event::new(EventAction::ImeChange, EventType::System).data(json!({
                "trigger": "registry_fingerprint",
                "layouts": layouts,
                "input_method_override": override_,
                "fingerprint": fingerprint,
            })),
        ),
    };
    (fingerprint, event)
}

/// 枚举 HKCU\Keyboard Layout\Preload 下的全部 REG_SZ 值（如 "00000409"）
fn enum_preload_layouts() -> Vec<String> {
    let mut result: Vec<String> = Vec::new();
    unsafe {
        let subkey = windows_sys::core::w!("Keyboard Layout\\Preload");
        let mut hkey: HANDLE = std::ptr::null_mut();
        if RegOpenKeyExW(HKEY_CURRENT_USER, subkey, 0, KEY_READ, &mut hkey) != 0 {
            return result;
        }

        let mut value_count: u32 = 0;
        let mut max_name_len: u32 = 0;
        let mut max_data_len: u32 = 0;
        if RegQueryInfoKeyW(
            hkey,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut value_count,
            &mut max_name_len,
            &mut max_data_len,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        ) != 0
        {
            RegCloseKey(hkey);
            return result;
        }

        let name_buf_len = max_name_len.max(1) + 1;
        let mut name_buf = vec![0u16; name_buf_len as usize];
        let data_buf_len = (max_data_len.max(1) + 1) as usize;

        for i in 0..value_count {
            let mut name_len = name_buf_len;
            let mut data_type: u32 = 0;
            let mut data_buf = vec![0u8; data_buf_len];
            let mut data_len = data_buf_len as u32;

            let st = RegEnumValueW(
                hkey,
                i,
                name_buf.as_mut_ptr(),
                &mut name_len,
                std::ptr::null_mut(),
                &mut data_type,
                data_buf.as_mut_ptr(),
                &mut data_len,
            );
            if st != 0 {
                continue;
            }
            if data_type != REG_SZ || data_len < 2 {
                continue;
            }
            // data_len 含结尾 NUL 的字节数
            let wide = &data_buf[..(data_len as usize / 2 - 1) * 2];
            let s = String::from_utf16_lossy(&to_u16_vec(wide));
            if !s.is_empty() {
                result.push(s);
            }
        }

        RegCloseKey(hkey);
    }
    result
}

/// 读取 HKCU\Control Panel\International\User Profile!InputMethodOverride
fn query_input_method_override() -> Option<String> {
    unsafe {
        let subkey = windows_sys::core::w!("Control Panel\\International\\User Profile");
        let mut hkey: HANDLE = std::ptr::null_mut();
        if RegOpenKeyExW(HKEY_CURRENT_USER, subkey, 0, KEY_READ, &mut hkey) != 0 {
            return None;
        }

        let name = windows_sys::core::w!("InputMethodOverride");
        let mut data_type: u32 = 0;
        let mut buf = vec![0u8; 512];
        let mut len = buf.len() as u32;

        let st = RegQueryValueExW(
            hkey,
            name,
            std::ptr::null_mut(),
            &mut data_type,
            buf.as_mut_ptr(),
            &mut len,
        );
        RegCloseKey(hkey);

        if st != 0 || data_type != REG_SZ || len < 2 {
            return None;
        }
        let wide = &buf[..(len as usize / 2 - 1) * 2];
        let s = String::from_utf16_lossy(&to_u16_vec(wide));
        if s.is_empty() {
            None
        } else {
            Some(s)
        }
    }
}

/// 把 u8 切片按 LE u16 解释（长度保证为 2 的倍数）
fn to_u16_vec(bytes: &[u8]) -> Vec<u16> {
    bytes
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preload_layouts_should_exist_on_windows() {
        // HKCU\Keyboard Layout\Preload 在所有正常 Windows 用户会话下都存在
        let layouts = enum_preload_layouts();
        assert!(
            !layouts.is_empty(),
            "Keyboard Layout\\Preload should contain at least one layout"
        );
    }

    #[test]
    fn first_collect_establishes_baseline_without_event() {
        let monitor = ImeMonitor::default();
        let (tx, rx) = crossbeam_channel::unbounded();
        monitor.collect(&tx);
        assert!(rx.try_recv().is_err(), "first collect must not emit");
        assert!(monitor.last_fingerprint.take().is_some());
    }

    #[test]
    fn fingerprint_change_emits_ime_change_event() {
        let layouts: Vec<String> = vec!["00000804".into(), "00000409".into()];
        let ov: Option<String> = None;

        let (fp1, ev1) = compute_change(None, &layouts, &ov);
        assert!(ev1.is_none());

        // 相同指纹：不产事件
        let (_, ev_same) = compute_change(Some(&fp1), &layouts, &ov);
        assert!(ev_same.is_none());

        // 布局列表变化：产 ime_change 事件
        let mut layouts2 = layouts.clone();
        layouts2.push("00000411".into());
        let (fp2, ev2) = compute_change(Some(&fp1), &layouts2, &ov);
        assert_ne!(fp1, fp2);
        let ev = ev2.expect("fingerprint change must emit ime_change");
        assert_eq!(ev.event_action, EventAction::ImeChange);
        assert_eq!(ev.event_action.to_string(), "ime_change");
        let data = ev.event_data.expect("payload must exist");
        assert_eq!(data["trigger"], "registry_fingerprint");
        assert!(data["layouts"].as_array().unwrap().len() == 3);
    }

    #[test]
    fn override_change_emits_event() {
        let layouts: Vec<String> = vec!["00000409".into()];
        let (fp1, _) = compute_change(None, &layouts, &None);
        let (_, ev) = compute_change(Some(&fp1), &layouts, &Some("0804:00000804".into()));
        assert!(ev.is_some(), "override change must emit ime_change");
    }
}
