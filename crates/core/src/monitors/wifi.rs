//! WiFi 连接状态监控（原生 Native WiFi API，零子进程）
//!
//! 通过 windows-sys 的 WlanOpenHandle / WlanEnumInterfaces / WlanQueryInterface
//! 读取 SSID 与信号质量。P0 子进程风暴修复：旧实现每 15s spawn 一次
//! `netsh wlan show interfaces`（每天 ~5760 次子进程），已整体移除。
//!
//! netsh 文本解析器（[`parse_wifi_interfaces`]）保留为诊断纯函数（含多语言
//! 单测），但 collect 路径不再依赖子进程。

use crate::types::*;
use serde_json::json;
use std::cell::Cell;
use std::time::Duration;

use windows_sys::Win32::Foundation::ERROR_SUCCESS;
use windows_sys::Win32::NetworkManagement::WiFi::{
    wlan_intf_opcode_current_connection, WlanCloseHandle, WlanEnumInterfaces, WlanFreeMemory,
    WlanOpenHandle, WlanQueryInterface, WLAN_CONNECTION_ATTRIBUTES, WLAN_INTERFACE_INFO_LIST,
};

pub struct WifiMonitor {
    prev_state: Cell<Option<String>>,
}

impl Default for WifiMonitor {
    fn default() -> Self {
        Self {
            prev_state: Cell::new(None),
        }
    }
}

impl Monitor for WifiMonitor {
    fn name(&self) -> &str {
        "wifi"
    }
    fn interval(&self) -> Duration {
        // 原生 API 调用开销极低（无子进程），保持 15s 变化检测延迟
        Duration::from_secs(15)
    }

    fn collect(&self, tx: &crossbeam_channel::Sender<Event>) {
        let info = match query_wifi_native() {
            Some(i) => i,
            // 原生查询失败（无无线网卡/WLAN 服务未运行等）：跳过本轮，不产事件
            None => return,
        };

        let state_key = format!("{}|{}", info.state, info.ssid);
        let prev = self.prev_state.take();
        if prev.as_ref() == Some(&state_key) {
            self.prev_state.set(prev);
            return;
        }
        self.prev_state.set(Some(state_key));

        let event = Event::new(EventAction::WifiChange, EventType::Network).data(json!({
            "state": info.state,
            "ssid": info.ssid,
            "signal_pct": info.signal,
            "speed": info.speed,
        }));
        let _ = tx.try_send(event);
    }
}

/// WiFi 状态（与旧 netsh 解析路径同构，state/ssid/signal/speed）。
#[derive(Debug, Default, PartialEq)]
pub struct WifiInfo {
    pub state: String,
    pub ssid: String,
    pub signal: String,
    pub speed: String,
}

/// 原生 Native WiFi API 查询当前连接（任一无线接口已连接则返回其信息）。
///
/// 接口存在但均未连接 → 返回 state="disconnected"（与旧 netsh 路径一致，
/// 断开也能产 WifiChange 事件）。整体失败（打开句柄/枚举失败，例如无无线
/// 网卡或 WLAN 服务未运行）返回 None → collect 跳过本轮。
fn query_wifi_native() -> Option<WifiInfo> {
    unsafe {
        let mut negotiated: u32 = 0;
        let mut handle = std::ptr::null_mut();
        if WlanOpenHandle(2, std::ptr::null(), &mut negotiated, &mut handle) != ERROR_SUCCESS as u32 {
            return None;
        }

        let mut list: *mut WLAN_INTERFACE_INFO_LIST = std::ptr::null_mut();
        let mut result: Option<WifiInfo> = None;
        if WlanEnumInterfaces(handle, std::ptr::null(), &mut list) == ERROR_SUCCESS as u32 && !list.is_null() {
            // 枚举成功但没有任何已连接接口 → 明确的 disconnected 状态
            let mut connected = false;
            let count = (*list).dwNumberOfItems as usize;
            let items = (*list).InterfaceInfo.as_ptr();
            for i in 0..count {
                let iface = &*items.add(i);
                let mut data_size: u32 = 0;
                let mut data: *mut core::ffi::c_void = std::ptr::null_mut();
                if WlanQueryInterface(
                    handle,
                    &iface.InterfaceGuid,
                    wlan_intf_opcode_current_connection,
                    std::ptr::null(),
                    &mut data_size,
                    &mut data,
                    std::ptr::null_mut(),
                ) == ERROR_SUCCESS as u32
                    && !data.is_null()
                {
                    let attrs = &*(data as *const WLAN_CONNECTION_ATTRIBUTES);
                    let assoc = &attrs.wlanAssociationAttributes;
                    let ssid_len = assoc.dot11Ssid.uSSIDLength as usize;
                    let ssid: String = String::from_utf8_lossy(
                        &assoc.dot11Ssid.ucSSID[..ssid_len.min(32)],
                    )
                    .into_owned();
                    // wlanSignalQuality 为 0-100 的信号强度百分比
                    result = Some(WifiInfo {
                        state: "connected".to_string(),
                        ssid,
                        signal: format!("{}%", assoc.wlanSignalQuality),
                        speed: String::new(),
                    });
                    WlanFreeMemory(data as *const _);
                    connected = true;
                    break; // 取第一个已连接接口即可
                }
            }
            WlanFreeMemory(list as *const _);
            if !connected {
                result = Some(WifiInfo {
                    state: "disconnected".to_string(),
                    ssid: String::new(),
                    signal: String::new(),
                    speed: String::new(),
                });
            }
        }
        WlanCloseHandle(handle, std::ptr::null());
        result
    }
}

/// 纯函数：解析 `netsh wlan show interfaces` 文本输出（保留的诊断路径，
/// collect 已改走原生 API，不再 spawn netsh）。
///
/// 中英键名都兼容（State/状态、Signal/信号、Receive rate/接收速率 等），
/// 便于单元测试多语言/空输出/disconnected 等场景。
pub fn parse_wifi_interfaces(text: &str) -> WifiInfo {
    let mut info = WifiInfo::default();
    for line in text.lines() {
        if let Some((k, v)) = line.split_once(':') {
            let key = k.trim();
            let val = v.trim().to_string();
            match key {
                "State" | "状态" => info.state = val,
                "SSID" if !val.is_empty() => info.ssid = val,
                "Signal" | "信号" => info.signal = val,
                "Receive rate" | "Transmit rate" | "接收速率" | "传输速率" if !val.is_empty() =>
                {
                    info.speed = val;
                }
                _ => {}
            }
        }
    }
    info
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_wifi_english_connected() {
        let out = "\
There is 1 interface on the system:

    Name                   : WLAN
    State                  : connected
    SSID                   : MyHomeWiFi
    Signal                 : 87%
    Receive rate           : 866.7 Mbps
";
        let info = parse_wifi_interfaces(out);
        assert_eq!(info.state, "connected");
        assert_eq!(info.ssid, "MyHomeWiFi");
        assert_eq!(info.signal, "87%");
        assert_eq!(info.speed, "866.7 Mbps");
    }

    #[test]
    fn parse_wifi_chinese_connected() {
        let out = "\
    状态                  : 已连接
    SSID                   : 我的WiFi
    信号                  : 92%
    接收速率              : 1.2 Gbps
";
        let info = parse_wifi_interfaces(out);
        assert_eq!(info.state, "已连接");
        assert_eq!(info.ssid, "我的WiFi");
        assert_eq!(info.signal, "92%");
        assert_eq!(info.speed, "1.2 Gbps");
    }

    #[test]
    fn parse_wifi_disconnected() {
        let out = "    State                  : disconnected\n    SSID                   : \n";
        let info = parse_wifi_interfaces(out);
        assert_eq!(info.state, "disconnected");
        // SSID 为空时应保持默认（空串），不被覆盖成空串也算正确
        assert_eq!(info.ssid, "");
    }

    #[test]
    fn parse_wifi_empty() {
        let info = parse_wifi_interfaces("");
        assert_eq!(info, WifiInfo::default());
    }
}
