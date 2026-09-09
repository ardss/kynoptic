//! 托盘状态机与菜单 id(纯逻辑,单测覆盖;不含 Win32 调用)。

/// 托盘菜单项 id(传给 WM_COMMAND / TrackPopupMenu 的命令码)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum MenuId {
    OpenDashboard = 1001,
    TogglePause = 1002,
    OpenDataFolder = 1003,
    Quit = 1004,
}

impl MenuId {
    /// 从 WM_COMMAND 的命令码还原;未知命令码返回 None(忽略)。
    pub fn from_command(code: u32) -> Option<Self> {
        match code {
            1001 => Some(Self::OpenDashboard),
            1002 => Some(Self::TogglePause),
            1003 => Some(Self::OpenDataFolder),
            1004 => Some(Self::Quit),
            _ => None,
        }
    }

    /// 当前状态下的菜单文案(无 emoji;Pause/Resume 随状态切换)。
    pub fn label(self, state: TrayState) -> &'static str {
        match self {
            Self::OpenDashboard => "Open Dashboard",
            Self::TogglePause => match state {
                TrayState::Running => "Pause",
                TrayState::Paused | TrayState::Error => "Resume",
            },
            Self::OpenDataFolder => "Open data folder",
            Self::Quit => "Quit",
        }
    }
}

/// 托盘三态(对应三个运行时绘制的图标)。
///
/// v0.1 通知面尚未接线:Error 态与 transition/StateEvent 目前仅被单测
/// 驱动,保留作为 dashboard 服务异常反馈的纯逻辑基座(见 state.rs 测试)。
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayState {
    /// 采集中 = 实心圆
    Running,
    /// 暂停 = 空心圆
    Paused,
    /// 异常 = 黄三角
    Error,
}

impl TrayState {
    /// 状态迁移规则
    #[allow(dead_code)] // v0.1 仅单测驱动,预留给服务异常反馈接线(Pause/Resume/出错/恢复采集)。
    pub fn transition(self, ev: StateEvent) -> Self {
        match (self, ev) {
            (s, StateEvent::CollectionStarted) => {
                let _ = s; // 任意状态启动采集都回到 Running
                Self::Running
            }
            (_, StateEvent::CollectionPaused) => Self::Paused,
            (_, StateEvent::ServiceFailed) => Self::Error,
            (Self::Error, StateEvent::ServiceRecovered) => Self::Running,
            (s, StateEvent::ServiceRecovered) => s,
        }
    }
}

/// 状态迁移事件。
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateEvent {
    CollectionStarted,
    CollectionPaused,
    ServiceFailed,
    ServiceRecovered,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menu_ids_round_trip() {
        for id in [
            MenuId::OpenDashboard,
            MenuId::TogglePause,
            MenuId::OpenDataFolder,
            MenuId::Quit,
        ] {
            assert_eq!(MenuId::from_command(id as u32), Some(id));
        }
        assert_eq!(MenuId::from_command(0), None);
        assert_eq!(MenuId::from_command(9999), None);
    }

    #[test]
    fn pause_resume_labels_follow_state() {
        assert_eq!(MenuId::TogglePause.label(TrayState::Running), "Pause");
        assert_eq!(MenuId::TogglePause.label(TrayState::Paused), "Resume");
        assert_eq!(MenuId::TogglePause.label(TrayState::Error), "Resume");
        assert_eq!(
            MenuId::OpenDashboard.label(TrayState::Running),
            "Open Dashboard"
        );
        assert_eq!(MenuId::Quit.label(TrayState::Paused), "Quit");
        assert_eq!(
            MenuId::OpenDataFolder.label(TrayState::Running),
            "Open data folder"
        );
    }

    #[test]
    fn state_transitions() {
        // 正常循环:启动 -> 暂停 -> 恢复
        let s = TrayState::Paused;
        assert_eq!(
            s.transition(StateEvent::CollectionStarted),
            TrayState::Running
        );
        let s = TrayState::Running;
        assert_eq!(
            s.transition(StateEvent::CollectionPaused),
            TrayState::Paused
        );

        // 异常盖过一切状态
        assert_eq!(
            TrayState::Running.transition(StateEvent::ServiceFailed),
            TrayState::Error
        );
        assert_eq!(
            TrayState::Paused.transition(StateEvent::ServiceFailed),
            TrayState::Error
        );

        // 恢复:Error 回到 Running;非 Error 保持
        assert_eq!(
            TrayState::Error.transition(StateEvent::ServiceRecovered),
            TrayState::Running
        );
        assert_eq!(
            TrayState::Paused.transition(StateEvent::ServiceRecovered),
            TrayState::Paused
        );

        // 启动采集从 Error 也能回 Running
        assert_eq!(
            TrayState::Error.transition(StateEvent::CollectionStarted),
            TrayState::Running
        );
    }
}
