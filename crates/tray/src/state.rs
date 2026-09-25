//! 托盘状态机与菜单 id(纯逻辑,单测覆盖;不含 Win32 调用)。

/// 托盘菜单项 id(传给 WM_COMMAND / TrackPopupMenu 的命令码)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum MenuId {
    OpenDashboard = 1001,
    TogglePause = 1002,
    OpenDataFolder = 1003,
    /// 自动更新检查发现新版本后动态插入的一键更新/下载项
    UpdateNow = 1005,
    /// 关于（打开项目主页；全应用唯一的版本可见入口之一）
    About = 1006,
    /// 一键更新未能启动（update-error.txt 在场）的提示项：点击打开数据目录
    UpdateError = 1007,
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
            1005 => Some(Self::UpdateNow),
            1006 => Some(Self::About),
            1007 => Some(Self::UpdateError),
            _ => None,
        }
    }

    /// 当前状态下的菜单文案(Wave17 双语化:托盘是常驻可见面)。
    /// 双语规范（审查 low）：全产品统一「中文 / English」——中文在前，
    /// 半角「 / 」分隔（安装器消息、postinstall 同规则，ci 域维护）。
    pub fn label(self, state: TrayState) -> &'static str {
        match self {
            Self::OpenDashboard => "打开面板 / Open Dashboard",
            Self::TogglePause => match state {
                TrayState::Running => "暂停采集 / Pause",
                TrayState::Paused | TrayState::Error => "恢复采集 / Resume",
            },
            Self::OpenDataFolder => "打开数据目录 / Open data folder",
            // 版本号动态拼在调用侧(见 update_menu_label;此处仅测试锚点)
            Self::UpdateNow => "发现新版本 / Update available",
            Self::UpdateError => "更新未能启动，查看原因 / Update did not start, view reason",
            Self::About => "关于 Kynoptic (github) / About",
            Self::Quit => "退出 / Quit",
        }
    }

    /// Error 态的恢复项文案（按故障来源分：面板故障时 Resume 修不了面板，
    /// 必须如实说，否则点了没反应还伴随图标闪烁）。
    pub fn resume_label(&self, dash_failed: bool) -> &'static str {
        if dash_failed {
            "面板不可用（详见数据目录 dashboard-error.log） / Dashboard unavailable (see dashboard-error.log in the data folder)"
        } else {
            "恢复采集 / Resume"
        }
    }

    /// 面板故障态的"打开面板"项文案（平台审查：此态下点击必然打开拒绝
    /// 连接的死链，行为实为打开数据目录查看原因，文案必须如实）。
    pub fn dashboard_label(dash_failed: bool) -> &'static str {
        if dash_failed {
            "面板不可用，查看原因 / Dashboard unavailable, view reason"
        } else {
            "打开面板 / Open Dashboard"
        }
    }

    /// 一键更新菜单项文案(单一来源,Wave17 P1:此前调用侧自拼文案与
    /// 此处措辞漂移)。安装版只能打开下载页,不做虚假的 "install" 承诺。
    pub fn update_menu_label(version: &str, installed: bool) -> String {
        if installed {
            format!("新版本 v{version} - 打开下载页 / New version v{version} - open download page")
        } else {
            format!("更新到 v{version} / Update to v{version}")
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
            MenuId::UpdateError,
        ] {
            assert_eq!(MenuId::from_command(id as u32), Some(id));
        }
        assert_eq!(MenuId::from_command(0), None);
        assert_eq!(MenuId::from_command(9999), None);
    }

    #[test]
    fn error_resume_label_says_dashboard_when_dash_failed() {
        // 审查 medium：面板故障时菜单不得许诺"恢复采集"
        let l = MenuId::TogglePause.resume_label(true);
        assert!(l.contains("面板不可用") && l.contains("dashboard-error.log"));
        assert!(!l.contains("恢复采集"));
        let r = MenuId::TogglePause.resume_label(false);
        assert!(r.contains("恢复采集"));
        // 双语规范：中文在前、半角 " / " 分隔
        for label in [
            MenuId::OpenDashboard.label(TrayState::Running),
            MenuId::Quit.label(TrayState::Paused),
        ] {
            let zh = label.split(" / ").next().unwrap_or("");
            assert!(!zh.is_empty() && !zh.is_ascii(), "{label} 应中文在前");
        }
    }

    #[test]
    fn pause_resume_labels_follow_state() {
        // Wave17 双语化后锚定关键词而非全文（文案仍可能微调，语义不变）
        for (st, kw) in [
            (TrayState::Running, "Pause"),
            (TrayState::Paused, "Resume"),
            (TrayState::Error, "Resume"),
        ] {
            assert!(
                MenuId::TogglePause.label(st).contains(kw),
                "{kw} 应出现在 {:?} 的菜单项里",
                st
            );
        }
        assert!(MenuId::OpenDashboard
            .label(TrayState::Running)
            .contains("Open Dashboard"));
        assert!(MenuId::Quit.label(TrayState::Paused).contains("Quit"));
        assert!(MenuId::OpenDataFolder
            .label(TrayState::Running)
            .contains("Open data folder"));
    }

    #[test]
    fn update_menu_label_matches_install_form() {
        // Wave17 P1：安装版不许再承诺 "install"（行为是打开下载页）
        let l = MenuId::update_menu_label("0.2.1", true);
        assert!(l.contains("0.2.1") && l.contains("download"));
        assert!(!l.to_lowercase().contains("install"));
        let p = MenuId::update_menu_label("0.2.1", false);
        assert!(p.contains("0.2.1") && p.to_lowercase().contains("update"));
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
