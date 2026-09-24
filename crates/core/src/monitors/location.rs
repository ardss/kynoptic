//! 位置快照监控（当前为占位：不采集）
//!
//! 【默认关闭】历史实现通过 PowerShell 调用 ipinfo.io，把用户公网 IP 发给
//! 第三方服务换取粗略位置——这违反 README「无网络调用（GitHub 版本检查除外）」
//! 的隐私承诺，外呼已移除。
//!
//! 待原生定位 API（Windows 定位服务）实现、且在设置页向用户明确披露数据
//! 去向之后再考虑恢复采集。在此之前本监控器保持注册（配置兼容）但不产生
//! 任何事件、不做任何网络访问。

use crate::types::*;
use std::time::Duration;

pub struct LocationMonitor;

impl Default for LocationMonitor {
    fn default() -> Self {
        Self
    }
}

impl Monitor for LocationMonitor {
    fn name(&self) -> &str {
        "location"
    }
    fn interval(&self) -> Duration {
        Duration::from_secs(300)
    }

    fn collect(&self, _tx: &crossbeam_channel::Sender<Event>) {
        // 占位：不做任何采集，不产生事件，不发起网络请求（见模块注释）。
    }
}
