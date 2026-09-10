//! WiFi 连接状态监控
//!
//! 通过 `netsh wlan show interfaces` 检测 WiFi 状态变化。
//!
//! netsh 的输出键名随系统语言变化（英文 `State`/`Signal`，中文 `状态`/`信号`），
//! 这里同时匹配中英两种键名，避免在中文 Windows 下字段全部为空。

use crate::types::*;
use serde_json::json;
use std::cell::Cell;
use std::time::Duration;

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
        Duration::from_secs(15)
    }

    fn collect(&self, tx: &crossbeam_channel::Sender<Event>) {
        let output = match std::process::Command::new("netsh")
            .args(["wlan", "show", "interfaces"])
            .output()
        {
            Ok(o) => String::from_utf8_lossy(&o.stdout).to_string(),
            Err(_) => return,
        };

        let info = parse_wifi_interfaces(&output);

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

/// 解析 `netsh wlan show interfaces` 得到的 WiFi 状态。
#[derive(Debug, Default, PartialEq)]
pub struct WifiInfo {
    pub state: String,
    pub ssid: String,
    pub signal: String,
    pub speed: String,
}

/// 纯函数：解析 `netsh wlan show interfaces` 文本输出。
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
