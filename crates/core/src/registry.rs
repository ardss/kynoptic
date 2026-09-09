//! 监控器注册表：40 个监控器的默认开关、敏感度与依赖分类的**单一事实源**。
//!
//! 历史脉络：v0.1 按《R8-v01采集裁剪.md》只保留了 14 个监控器；用户决策
//! （2026-09-09）恢复上游全部 40 个——"已经写了的代码不应该裁；敏感的默认关"。
//! 因此默认启用集合仍精确等于 v0.1 的 14 个（BENCHMARKS.md 空载基准继续有效），
//! 恢复的 26 个全部默认关闭，可通过 [`MonitorOverrides`] 按需启用。
//!
//! 依赖分类：
//! - [`Dep::Native`]：纯 windows-sys，零子进程
//! - [`Dep::PowerShell`]：每次采集 spawn 一个 powershell（慢、易触发杀软），
//!   默认一律关闭；代码标注"PS 子进程，待原生重写"
//!
//! `crates/core/config/monitors.json` 是随仓库分发的默认配置模板，
//! 测试保证其内容与本注册表逐项一致。

use std::collections::HashSet;

/// 监控器实现依赖类别
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dep {
    /// 纯 windows-sys 原生 API，零子进程
    Native,
    /// 通过 monitors/ps.rs spawn PowerShell 子进程（WMI/CIM/事件日志）
    PowerShell,
}

impl Dep {
    pub fn as_str(&self) -> &'static str {
        match self {
            Dep::Native => "native",
            Dep::PowerShell => "powershell",
        }
    }
}

/// 敏感度分级（沿 R8 的 ①/②/③ 分层）
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Sensitivity {
    /// R8 ①：低敏感，默认启用
    Low,
    /// R8 ②：中敏感（浏览/剪贴板/文件/设备清单等），带隐私开关语义，默认关闭
    Medium,
    /// R8 ③：高敏感或主题外（位置/DNS/安全审计等），默认关闭
    High,
}

impl Sensitivity {
    pub fn as_str(&self) -> &'static str {
        match self {
            Sensitivity::Low => "low",
            Sensitivity::Medium => "medium",
            Sensitivity::High => "high",
        }
    }
}

/// 单个监控器的注册项
#[derive(Debug, Clone, Copy)]
pub struct MonitorSpec {
    /// 稳定 id（= Monitor::name() / Hook 名，同时是 config 模板键）
    pub id: &'static str,
    /// 是否默认启用（默认启用集合必须精确等于 v0.1 的 14 个）
    pub default_enabled: bool,
    pub sensitivity: Sensitivity,
    pub dep: Dep,
}

/// 全量注册表：14 默认启用 + 26 恢复默认关闭 = 40。
pub const MONITOR_REGISTRY: &[MonitorSpec] = &[
    // ── v0.1 默认启用（14，全部 Native，低敏感）──
    MonitorSpec {
        id: "window",
        default_enabled: true,
        sensitivity: Sensitivity::Low,
        dep: Dep::Native,
    },
    MonitorSpec {
        id: "keyboard_hook",
        default_enabled: true,
        sensitivity: Sensitivity::Low,
        dep: Dep::Native,
    },
    MonitorSpec {
        id: "mouse_hook",
        default_enabled: true,
        sensitivity: Sensitivity::Low,
        dep: Dep::Native,
    },
    MonitorSpec {
        id: "idle",
        default_enabled: true,
        sensitivity: Sensitivity::Low,
        dep: Dep::Native,
    },
    MonitorSpec {
        id: "session",
        default_enabled: true,
        sensitivity: Sensitivity::Low,
        dep: Dep::Native,
    },
    MonitorSpec {
        id: "system",
        default_enabled: true,
        sensitivity: Sensitivity::Low,
        dep: Dep::Native,
    },
    MonitorSpec {
        id: "network",
        default_enabled: true,
        sensitivity: Sensitivity::Low,
        dep: Dep::Native,
    },
    MonitorSpec {
        id: "battery",
        default_enabled: true,
        sensitivity: Sensitivity::Low,
        dep: Dep::Native,
    },
    MonitorSpec {
        id: "device",
        default_enabled: true,
        sensitivity: Sensitivity::Low,
        dep: Dep::Native,
    },
    MonitorSpec {
        id: "process",
        default_enabled: true,
        sensitivity: Sensitivity::Low,
        dep: Dep::Native,
    },
    MonitorSpec {
        id: "audio",
        default_enabled: true,
        sensitivity: Sensitivity::Low,
        dep: Dep::Native,
    },
    MonitorSpec {
        id: "brightness",
        default_enabled: true,
        sensitivity: Sensitivity::Low,
        dep: Dep::Native,
    },
    MonitorSpec {
        id: "wifi",
        default_enabled: true,
        sensitivity: Sensitivity::Low,
        dep: Dep::Native,
    },
    MonitorSpec {
        id: "power_plan",
        default_enabled: true,
        sensitivity: Sensitivity::Low,
        dep: Dep::Native,
    },
    // ── R8 ② 恢复（12，中敏感，默认关闭）──
    MonitorSpec {
        id: "browser",
        default_enabled: false,
        sensitivity: Sensitivity::Medium,
        dep: Dep::Native,
    },
    MonitorSpec {
        id: "clipboard",
        default_enabled: false,
        sensitivity: Sensitivity::Medium,
        dep: Dep::Native,
    },
    MonitorSpec {
        id: "file_activity",
        default_enabled: false,
        sensitivity: Sensitivity::Medium,
        dep: Dep::Native,
    },
    MonitorSpec {
        id: "media",
        default_enabled: false,
        sensitivity: Sensitivity::Medium,
        dep: Dep::Native,
    },
    MonitorSpec {
        id: "screen_capture",
        default_enabled: false,
        sensitivity: Sensitivity::Medium,
        dep: Dep::Native,
    },
    MonitorSpec {
        id: "usb_device",
        default_enabled: false,
        sensitivity: Sensitivity::Medium,
        dep: Dep::Native,
    },
    MonitorSpec {
        id: "bluetooth",
        default_enabled: false,
        sensitivity: Sensitivity::Medium,
        dep: Dep::Native,
    },
    MonitorSpec {
        id: "display",
        default_enabled: false,
        sensitivity: Sensitivity::Medium,
        dep: Dep::PowerShell,
    },
    MonitorSpec {
        id: "external_display",
        default_enabled: false,
        sensitivity: Sensitivity::Medium,
        dep: Dep::PowerShell,
    },
    MonitorSpec {
        id: "audio_input",
        default_enabled: false,
        sensitivity: Sensitivity::Medium,
        dep: Dep::PowerShell,
    },
    MonitorSpec {
        id: "audio_output",
        default_enabled: false,
        sensitivity: Sensitivity::Medium,
        dep: Dep::PowerShell,
    },
    MonitorSpec {
        id: "ime",
        default_enabled: false,
        sensitivity: Sensitivity::Medium,
        dep: Dep::PowerShell,
    },
    // ── R8 ③ 恢复（14，高敏感或主题外，默认关闭）──
    MonitorSpec {
        id: "location",
        default_enabled: false,
        sensitivity: Sensitivity::High,
        dep: Dep::PowerShell,
    },
    MonitorSpec {
        id: "notification",
        default_enabled: false,
        sensitivity: Sensitivity::High,
        dep: Dep::PowerShell,
    },
    MonitorSpec {
        id: "calendar",
        default_enabled: false,
        sensitivity: Sensitivity::High,
        dep: Dep::PowerShell,
    },
    MonitorSpec {
        id: "dns",
        default_enabled: false,
        sensitivity: Sensitivity::High,
        dep: Dep::PowerShell,
    },
    MonitorSpec {
        id: "security",
        default_enabled: false,
        sensitivity: Sensitivity::High,
        dep: Dep::PowerShell,
    },
    MonitorSpec {
        id: "firewall",
        default_enabled: false,
        sensitivity: Sensitivity::High,
        dep: Dep::PowerShell,
    },
    MonitorSpec {
        id: "uac",
        default_enabled: false,
        sensitivity: Sensitivity::High,
        dep: Dep::PowerShell,
    },
    MonitorSpec {
        id: "windows_update",
        default_enabled: false,
        sensitivity: Sensitivity::High,
        dep: Dep::PowerShell,
    },
    MonitorSpec {
        id: "driver",
        default_enabled: false,
        sensitivity: Sensitivity::High,
        dep: Dep::PowerShell,
    },
    MonitorSpec {
        id: "vpn",
        default_enabled: false,
        sensitivity: Sensitivity::High,
        dep: Dep::PowerShell,
    },
    MonitorSpec {
        id: "print",
        default_enabled: false,
        sensitivity: Sensitivity::High,
        dep: Dep::PowerShell,
    },
    MonitorSpec {
        id: "stylus",
        default_enabled: false,
        sensitivity: Sensitivity::High,
        dep: Dep::PowerShell,
    },
    MonitorSpec {
        id: "thermal",
        default_enabled: false,
        sensitivity: Sensitivity::High,
        dep: Dep::PowerShell,
    },
    MonitorSpec {
        id: "gpu",
        default_enabled: false,
        sensitivity: Sensitivity::High,
        dep: Dep::PowerShell,
    },
];

/// 默认启用集合的 id 列表（顺序与注册表一致）。
/// 全部 40 个监控器 id（= MONITOR_REGISTRY 全集）。
pub fn all_monitor_ids() -> Vec<&'static str> {
    MONITOR_REGISTRY.iter().map(|s| s.id).collect()
}

pub fn default_enabled_ids() -> Vec<&'static str> {
    MONITOR_REGISTRY
        .iter()
        .filter(|s| s.default_enabled)
        .map(|s| s.id)
        .collect()
}

/// 按 id 集合构建启用的轮询型监控器实例（不含 Hook；Hook 由
/// [`crate::collector::create_hooks`] 单独管理）。
/// 未识别的 id 忽略并 log::warn，便于配置容错。
pub fn create_monitors_for(
    enabled: &HashSet<String>,
) -> Vec<Box<dyn crate::types::Monitor + Send>> {
    use crate::monitors;
    use crate::types::Monitor;

    let mut out: Vec<Box<dyn Monitor + Send>> = Vec::new();
    let want = |id: &str| enabled.contains(id);

    // 注意：分支顺序即注册表顺序，新增监控器时同步维护 MONITOR_REGISTRY。
    if want("window") {
        out.push(Box::new(monitors::window::WindowMonitor::default()));
    }
    if want("idle") {
        out.push(Box::new(monitors::idle::IdleMonitor::default()));
    }
    if want("session") {
        out.push(Box::new(monitors::session::SessionMonitor::default()));
    }
    if want("browser") {
        out.push(Box::new(monitors::browser::BrowserMonitor::default()));
    }
    if want("media") {
        out.push(Box::new(monitors::media::MediaMonitor::default()));
    }
    if want("file_activity") {
        out.push(Box::new(
            monitors::file_activity::FileActivityMonitor::default(),
        ));
    }
    if want("audio") {
        out.push(Box::new(monitors::audio::AudioMonitor::default()));
    }
    if want("bluetooth") {
        out.push(Box::new(monitors::bluetooth::BluetoothMonitor::default()));
    }
    if want("brightness") {
        out.push(Box::new(monitors::brightness::BrightnessMonitor::default()));
    }
    if want("process") {
        out.push(Box::new(monitors::process::ProcessMonitor::default()));
    }
    if want("system") {
        out.push(Box::new(monitors::system::SystemMonitor));
    }
    if want("device") {
        out.push(Box::new(monitors::device::DeviceMonitor::default()));
    }
    if want("network") {
        out.push(Box::new(monitors::network::NetworkMonitor::default()));
    }
    if want("clipboard") {
        out.push(Box::new(monitors::clipboard::ClipboardMonitor::default()));
    }
    if want("battery") {
        out.push(Box::new(monitors::battery::BatteryMonitor::default()));
    }
    if want("power_plan") {
        out.push(Box::new(monitors::power_plan::PowerPlanMonitor::default()));
    }
    if want("gpu") {
        out.push(Box::new(monitors::gpu::GpuMonitor::default()));
    }
    if want("thermal") {
        out.push(Box::new(monitors::thermal::ThermalMonitor::default()));
    }
    if want("display") {
        out.push(Box::new(monitors::display::DisplayMonitor::default()));
    }
    if want("external_display") {
        out.push(Box::new(
            monitors::external_display::ExternalDisplayMonitor::default(),
        ));
    }
    if want("screen_capture") {
        out.push(Box::new(
            monitors::screen_capture::ScreenCaptureMonitor::default(),
        ));
    }
    if want("wifi") {
        out.push(Box::new(monitors::wifi::WifiMonitor::default()));
    }
    if want("dns") {
        out.push(Box::new(monitors::dns::DnsMonitor::default()));
    }
    if want("vpn") {
        out.push(Box::new(monitors::vpn::VpnMonitor::default()));
    }
    if want("firewall") {
        out.push(Box::new(monitors::firewall::FirewallMonitor::default()));
    }
    if want("security") {
        out.push(Box::new(monitors::security::SecurityMonitor::default()));
    }
    if want("uac") {
        out.push(Box::new(monitors::uac::UacMonitor::default()));
    }
    if want("windows_update") {
        out.push(Box::new(
            monitors::windows_update::WindowsUpdateMonitor::default(),
        ));
    }
    if want("driver") {
        out.push(Box::new(monitors::driver::DriverMonitor::default()));
    }
    if want("usb_device") {
        out.push(Box::new(monitors::usb_device::UsbDeviceMonitor::default()));
    }
    if want("stylus") {
        out.push(Box::new(monitors::stylus::StylusMonitor::default()));
    }
    if want("audio_input") {
        out.push(Box::new(monitors::audio_input::AudioInputMonitor::default()));
    }
    if want("audio_output") {
        out.push(Box::new(
            monitors::audio_output::AudioOutputMonitor::default(),
        ));
    }
    if want("print") {
        out.push(Box::new(monitors::print::PrintMonitor::default()));
    }
    if want("calendar") {
        out.push(Box::new(monitors::calendar::CalendarMonitor::default()));
    }
    if want("location") {
        out.push(Box::new(monitors::location::LocationMonitor::default()));
    }
    if want("notification") {
        out.push(Box::new(
            monitors::notification::NotificationMonitor::default(),
        ));
    }
    if want("ime") {
        out.push(Box::new(monitors::ime::ImeMonitor::default()));
    }

    // 配置里写了但注册表未知的 id：提示（防拼写错误静默失效）
    for id in enabled {
        if !MONITOR_REGISTRY.iter().any(|s| s.id == id) {
            log::warn!("未知监控器 id: {id}（忽略）");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// 铁律校验：默认启用集合必须精确等于 v0.1 的 14 个
    /// （BENCHMARKS.md 空载基准的前提）。
    #[test]
    fn default_enabled_is_exactly_the_v01_fourteen() {
        let mut ids = default_enabled_ids();
        ids.sort_unstable();
        let mut expected = vec![
            "audio",
            "battery",
            "brightness",
            "device",
            "idle",
            "keyboard_hook",
            "mouse_hook",
            "network",
            "power_plan",
            "process",
            "session",
            "system",
            "wifi",
            "window",
        ];
        expected.sort_unstable();
        assert_eq!(
            ids, expected,
            "默认启用集合必须精确等于 v0.1 的 14 个监控器"
        );
    }

    #[test]
    fn registry_has_forty_unique_ids() {
        assert_eq!(MONITOR_REGISTRY.len(), 40);
        let mut ids: Vec<_> = MONITOR_REGISTRY.iter().map(|s| s.id).collect();
        let n = ids.len();
        ids.dedup();
        assert_eq!(ids.len(), n, "注册表 id 必须唯一");
    }

    /// 恢复的 26 个必须全部默认关闭（用户决策：敏感的默认关）。
    #[test]
    fn restored_monitors_all_default_off() {
        let restored: Vec<_> = MONITOR_REGISTRY
            .iter()
            .filter(|s| !s.default_enabled)
            .collect();
        assert_eq!(restored.len(), 26);
        // PS 依赖的监控器绝不默认启用（零子进程约束）
        for s in MONITOR_REGISTRY {
            if s.dep == Dep::PowerShell {
                assert!(!s.default_enabled, "{} 是 PS 依赖却默认启用", s.id);
            }
        }
    }

    /// 注册表默认配置构建出的监控器实例数 = 12（轮询型，不含 2 个 Hook），
    /// 且默认不产生任何 PS 依赖监控器。
    #[test]
    fn default_set_builds_twelve_polling_monitors() {
        let set: HashSet<String> = default_enabled_ids()
            .iter()
            .map(|s| s.to_string())
            .collect();
        let monitors = create_monitors_for(&set);
        assert_eq!(monitors.len(), 12, "默认 14 = 12 轮询 + 2 Hook");
    }

    /// 全量启用时 38 个轮询型监控器都能实例化（注册项与工厂分支一一对应）。
    #[test]
    fn registry_covers_every_polling_monitor() {
        let all: HashSet<String> = MONITOR_REGISTRY
            .iter()
            .filter(|s| s.id != "keyboard_hook" && s.id != "mouse_hook")
            .map(|s| s.id.to_string())
            .collect();
        assert_eq!(all.len(), 38);
        let monitors = create_monitors_for(&all);
        assert_eq!(monitors.len(), 38, "注册表与工厂分支数量不一致");
    }

    /// config/monitors.json 模板必须与注册表逐项一致（id/开关/敏感度/依赖）。
    #[test]
    fn config_template_matches_registry() {
        #[derive(serde::Deserialize)]
        struct Entry {
            id: String,
            default_enabled: bool,
            sensitivity: String,
            dep: String,
        }
        #[derive(serde::Deserialize)]
        struct Template {
            #[serde(rename = "readme")]
            #[allow(dead_code)]
            readme: Vec<String>,
            monitors: Vec<Entry>,
        }
        let raw = include_str!("../config/monitors.json");
        let tpl: Template = serde_json::from_str(raw).expect("monitors.json 解析失败");
        let entries = tpl.monitors;
        assert_eq!(entries.len(), MONITOR_REGISTRY.len());
        for (e, s) in entries.iter().zip(MONITOR_REGISTRY) {
            assert_eq!(e.id, s.id, "模板与注册表顺序错位");
            assert_eq!(
                e.default_enabled, s.default_enabled,
                "{}: 默认开关不一致",
                s.id
            );
            assert_eq!(
                e.sensitivity,
                s.sensitivity.as_str(),
                "{}: 敏感度不一致",
                s.id
            );
            assert_eq!(e.dep, s.dep.as_str(), "{}: 依赖类别不一致", s.id);
        }
    }
}
