//! 核心类型定义

use chrono::Utc;
use serde::Serialize;
use std::fmt;
use std::time::Duration;

/// 事件类型
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum EventType {
    Keyboard,
    Mouse,
    Window,
    System,
    Clipboard,
    Network,
    Session,
    Device,
    Location,
}

impl EventType {
    /// 返回事件类型的字符串标识（与 `Display` 一致，但返回 `&'static str` 无堆分配）。
    ///
    /// 用于 SQL 构造端（如 `WHERE event_type = ?` 绑定 `EventType::Keyboard.as_str()`），
    /// 替代散落各处的裸字符串字面量 `"keyboard"`，使枚举改名时编译期即可发现。
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Keyboard => "keyboard",
            Self::Mouse => "mouse",
            Self::Window => "window",
            Self::System => "system",
            Self::Clipboard => "clipboard",
            Self::Network => "network",
            Self::Session => "session",
            Self::Device => "device",
            Self::Location => "location",
        }
    }
}

impl fmt::Display for EventType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 事件动作
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum EventAction {
    // 输入
    Press,
    Release,
    Click,
    Scroll,
    Move,
    // 窗口
    Switch,
    TabChange,
    // 系统
    Heartbeat,
    AudioState,
    VolumeChange,
    BrightnessChange,
    MediaDevice,
    ProcessSnapshot,
    ScreenCapture,
    // 网络
    ConnSnapshot,
    WifiChange,
    DnsQuery,
    VpnChange,
    FirewallEvent,
    // 会话
    Lock,
    Unlock,
    DisplayChange,
    IdleStart,
    IdleEnd,
    // 设备
    DeviceSnapshot,
    BtChange,
    FileActivity,
    UsbDevice,
    StylusChange,
    DriverChange,
    // 剪贴板
    Change,
    // 电源
    BatteryStatus,
    PowerPlanChange,
    // GPU / 热力
    GpuSnapshot,
    ThermalSnapshot,
    // 显示
    ExternalDisplay,
    // 安全
    SecurityEvent,
    UacEvent,
    UpdateStatus,
    // 日历 / 位置
    CalendarEvent,
    LocationSnapshot,
    // 打印 / 通知 / IME
    PrintJob,
    Notification,
    ImeChange,
    // 音频
    AudioInputChange,
    AudioOutput,
}

impl EventAction {
    /// 返回事件动作的字符串标识（与 `Display` 一致，但返回 `&'static str` 无堆分配）。
    ///
    /// 用途同 [`EventType::as_str`]：消除 SQL 中的裸字符串字面量。
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Press => "press",
            Self::Release => "release",
            Self::Click => "click",
            Self::Scroll => "scroll",
            Self::Move => "move",
            Self::Switch => "switch",
            Self::TabChange => "tab_change",
            Self::Heartbeat => "heartbeat",
            Self::AudioState => "audio_state",
            Self::VolumeChange => "volume_change",
            Self::BrightnessChange => "brightness_change",
            Self::MediaDevice => "media_device",
            Self::ProcessSnapshot => "process_snapshot",
            Self::ScreenCapture => "screen_capture",
            Self::ConnSnapshot => "conn_snapshot",
            Self::WifiChange => "wifi_change",
            Self::DnsQuery => "dns_query",
            Self::VpnChange => "vpn_change",
            Self::FirewallEvent => "firewall_event",
            Self::Lock => "lock",
            Self::Unlock => "unlock",
            Self::DisplayChange => "display_change",
            Self::IdleStart => "idle_start",
            Self::IdleEnd => "idle_end",
            Self::DeviceSnapshot => "device_snapshot",
            Self::BtChange => "bt_change",
            Self::FileActivity => "file_activity",
            Self::UsbDevice => "usb_device",
            Self::StylusChange => "stylus_change",
            Self::DriverChange => "driver_change",
            Self::Change => "change",
            Self::BatteryStatus => "battery_status",
            Self::PowerPlanChange => "power_plan_change",
            Self::GpuSnapshot => "gpu_snapshot",
            Self::ThermalSnapshot => "thermal_snapshot",
            Self::ExternalDisplay => "external_display",
            Self::SecurityEvent => "security_event",
            Self::UacEvent => "uac_event",
            Self::UpdateStatus => "update_status",
            Self::CalendarEvent => "calendar_event",
            Self::LocationSnapshot => "location_snapshot",
            Self::PrintJob => "print_job",
            Self::Notification => "notification",
            Self::ImeChange => "ime_change",
            Self::AudioInputChange => "audio_input_change",
            Self::AudioOutput => "audio_output",
        }
    }
}

impl fmt::Display for EventAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 统一事件结构
#[derive(Debug, Clone)]
pub struct Event {
    pub timestamp: String,
    pub event_type: EventType,
    pub event_action: EventAction,
    pub event_data: Option<serde_json::Value>,
    pub app_name: Option<String>,
    pub window_title: Option<String>,
    pub session_id: Option<i64>,
}

impl Event {
    pub fn new(action: EventAction, etype: EventType) -> Self {
        Self {
            timestamp: Utc::now().to_rfc3339(),
            event_type: etype,
            event_action: action,
            event_data: None,
            app_name: None,
            window_title: None,
            session_id: None,
        }
    }

    pub fn data(mut self, data: serde_json::Value) -> Self {
        self.event_data = Some(data);
        self
    }

    pub fn app(mut self, name: impl Into<String>, title: impl Into<String>) -> Self {
        self.app_name = Some(name.into());
        self.window_title = Some(title.into());
        self
    }
}

/// Monitor trait — 所有轮询型监控器的统一接口
pub trait Monitor: Send {
    fn name(&self) -> &str;
    fn interval(&self) -> Duration;
    fn collect(&self, tx: &crossbeam_channel::Sender<Event>);
}

/// EventHook trait — 事件驱动型监控器（键盘/鼠标 Hook）
pub trait EventHook: Send + Sync {
    fn start(&self, tx: crossbeam_channel::Sender<Event>);
    fn stop(&self);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_type_display() {
        assert_eq!(EventType::Keyboard.to_string(), "keyboard");
        assert_eq!(EventType::Mouse.to_string(), "mouse");
        assert_eq!(EventType::Window.to_string(), "window");
        assert_eq!(EventType::Location.to_string(), "location");
    }

    #[test]
    fn event_action_display() {
        assert_eq!(EventAction::Press.to_string(), "press");
        assert_eq!(EventAction::Click.to_string(), "click");
        assert_eq!(EventAction::ThermalSnapshot.to_string(), "thermal_snapshot");
        assert_eq!(EventAction::AudioOutput.to_string(), "audio_output");
    }

    #[test]
    fn event_new_has_timestamp_and_defaults_none() {
        let e = Event::new(EventAction::Press, EventType::Keyboard);
        assert!(!e.timestamp.is_empty(), "timestamp 不应为空");
        assert!(e.event_data.is_none());
        assert!(e.app_name.is_none());
        assert!(e.window_title.is_none());
        assert!(e.session_id.is_none());
    }

    #[test]
    fn event_builder_chain() {
        let e = Event::new(EventAction::Switch, EventType::Window)
            .data(serde_json::json!({"title": "test"}))
            .app("chrome", "Google");
        assert_eq!(e.event_action, EventAction::Switch);
        assert_eq!(e.event_type, EventType::Window);
        assert_eq!(
            e.event_data.as_ref().unwrap()["title"],
            serde_json::json!("test")
        );
        assert_eq!(e.app_name.as_deref(), Some("chrome"));
        assert_eq!(e.window_title.as_deref(), Some("Google"));
    }

    #[test]
    fn event_data_overwrites() {
        // 多次调用 data 应覆盖
        let e = Event::new(EventAction::Click, EventType::Mouse)
            .data(serde_json::json!(1))
            .data(serde_json::json!(2));
        assert_eq!(e.event_data.unwrap(), serde_json::json!(2));
    }

    #[test]
    fn monitor_interval_positive_is_valid_contract() {
        // 仅验证 trait 契约：interval 返回的 Duration 可被构造
        // （具体监控器各自保证 > 0）
        let d = Duration::from_secs(1);
        assert!(!d.is_zero());
    }
}
