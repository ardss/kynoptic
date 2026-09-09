//! VPN 连接监控
//!
//! 【默认关闭】PS 子进程实现（powershell spawn），待原生 API 重写后再考虑默认启用。
//!
//! 通过 PowerShell Get-NetAdapter 检测 VPN 适配器状态变化。

use crate::monitors::ps::run_ps;
use crate::types::*;
use serde_json::json;
use std::cell::Cell;
use std::time::Duration;

pub struct VpnMonitor {
    prev_adapters: Cell<Option<String>>,
}

impl Default for VpnMonitor {
    fn default() -> Self {
        Self {
            prev_adapters: Cell::new(None),
        }
    }
}

impl Monitor for VpnMonitor {
    fn name(&self) -> &str {
        "vpn"
    }
    fn interval(&self) -> Duration {
        Duration::from_secs(30)
    }

    fn collect(&self, tx: &crossbeam_channel::Sender<Event>) {
        // Status 取值随系统语言变化（英文 Up/Disabled，中文 已连接/已断开连接），
        // 不能在 Rust 里硬编码匹配 "Up"。改在 PowerShell 里把 Status 归一化成
        // AdminStatus（bool，不受本地化影响）：管理员启用 = 连接候选。
        // 同时保留原 Status 文本供前端展示。
        let script = r#"
Get-NetAdapter | Where-Object {
  $_.InterfaceDescription -match 'vpn|tunnel|tap|wireguard|openvpn|wintun|tun'
} | ForEach-Object {
  # up_flag: 1 = 连接状态（兼容 Up/已连接/已连线 等本地化文本）
  $up = if ($_.Status -eq 'Up' -or $_.Status -match '连接|連線|Connected') { 1 } else { 0 }
  "$($_.Name)|$($_.Status)|$($_.InterfaceDescription)|$up"
}
"#;
        let output = match run_ps(script) {
            Some(o) => o,
            _ => return,
        };

        let mut adapters = Vec::new();
        let mut connected = Vec::new();
        let mut state_key = String::new();

        for line in output.trim().lines() {
            if line.trim().is_empty() {
                continue;
            }
            let parts: Vec<&str> = line.splitn(4, '|').collect();
            if parts.len() >= 4 {
                let name = parts[0].trim().to_string();
                let status = parts[1].trim().to_string();
                let desc = parts[2].trim().to_string();
                let up_flag = parts[3].trim() == "1";
                state_key.push_str(&format!("{}|{}|{}|", name, status, up_flag as u8));
                if up_flag {
                    connected.push(name.clone());
                }
                adapters.push(json!({
                    "name": name,
                    "status": status,
                    "description": desc,
                }));
            }
        }

        // 无 VPN 适配器时统一处理
        if state_key.is_empty() {
            let prev = self.prev_adapters.take();
            // 从有 VPN 变为无 VPN
            if prev.as_ref().is_some_and(|p| !p.is_empty()) {
                let event = Event::new(EventAction::VpnChange, EventType::Network).data(json!({
                    "action": "disconnected",
                    "adapters": [],
                    "connected_names": [],
                }));
                let _ = tx.try_send(event);
            }
            self.prev_adapters.set(Some(String::new()));
            return;
        }

        let prev = self.prev_adapters.take();
        if prev.as_ref() == Some(&state_key) {
            self.prev_adapters.set(prev);
            return;
        }
        self.prev_adapters.set(Some(state_key));

        let action = if connected.is_empty() {
            "disconnected"
        } else {
            "connected"
        };
        let event = Event::new(EventAction::VpnChange, EventType::Network).data(json!({
            "action": action,
            "adapters": adapters,
            "connected_names": connected,
        }));
        let _ = tx.try_send(event);
    }
}
