//! kynoptic-mcp —— MCP server 工具面（v0.1 stub）
//!
//! 依据《mcp-tool-spec-v1》定义工具面：多语义小工具、列表类工具必带 `limit`
//! （默认 20，上限 100）。本 crate 目前只实现工具注册表与参数校验的纯逻辑部分，
//! 传输层（stdio/协议编解码）后置。

use serde::{Deserialize, Serialize};

/// 规范中的 MCP 工具（不含 Resource）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tool {
    /// A. get_current_status —— 标量最小视图，热路径 0 SQL（ArcSwap 快照）
    GetCurrentStatus,
    /// B. get_summary —— 单指标聚合 + 昨日对比
    GetSummary,
    /// C. get_timeline —— 应用/窗口时间线段落
    GetTimeline,
    /// D. get_anomalies —— 异常事件列表
    GetAnomalies,
    /// E. wait_for —— 阻塞轮询兜底（MCP Tool 无推送语义）
    WaitFor,
}

impl Tool {
    pub fn name(&self) -> &'static str {
        match self {
            Tool::GetCurrentStatus => "get_current_status",
            Tool::GetSummary => "get_summary",
            Tool::GetTimeline => "get_timeline",
            Tool::GetAnomalies => "get_anomalies",
            Tool::WaitFor => "wait_for",
        }
    }

    pub fn all() -> &'static [Tool] {
        &[
            Tool::GetCurrentStatus,
            Tool::GetSummary,
            Tool::GetTimeline,
            Tool::GetAnomalies,
            Tool::WaitFor,
        ]
    }
}

/// 列表类工具的通用分页参数（spec：默认 20，上限 100）。
#[derive(Serialize, Deserialize)]
pub struct ListParams {
    #[serde(default = "default_limit")]
    pub limit: usize,
}

fn default_limit() -> usize {
    20
}

/// 钳制 limit 到规范允许区间 [1, 100]，缺省 20。
pub fn clamp_limit(limit: Option<usize>) -> usize {
    limit.unwrap_or_else(default_limit).clamp(1, 100)
}

impl Default for ListParams {
    fn default() -> Self {
        Self {
            limit: default_limit(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_surface_matches_spec() {
        let names: Vec<&str> = Tool::all().iter().map(|t| t.name()).collect();
        assert_eq!(
            names,
            [
                "get_current_status",
                "get_summary",
                "get_timeline",
                "get_anomalies",
                "wait_for"
            ]
        );
    }

    #[test]
    fn limit_clamped_per_spec() {
        assert_eq!(clamp_limit(None), 20, "缺省 20");
        assert_eq!(clamp_limit(Some(0)), 1, "下限 1");
        assert_eq!(clamp_limit(Some(50)), 50);
        assert_eq!(clamp_limit(Some(500)), 100, "上限 100");
    }

    #[test]
    fn list_params_default_is_spec_default() {
        let p: ListParams = serde_json::from_str("{}").unwrap();
        assert_eq!(p.limit, 20);
    }
}
