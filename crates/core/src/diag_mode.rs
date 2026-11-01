//! M4.2 诊断模式（有界、自动结束）。
//!
//! 定位：**默认关**的临时提粒开关——开启后采样间隔缩短、ETW 会话缓冲加大
//! （深诊档 profile），带硬上限（资源预算：≤2% 单核、RSS ≤64MiB）与到期
//! 自动结束恢复 Basic。全部时刻由调用方注入（禁真实时钟）。
//!
//! 铁律：有界——请求时长超过 [`DIAG_HARD_MAX_DURATION_MS`] 一律钳制；预算
//! 超限（[`enforce_budget`]）立即强制回 Basic 并留原因；本模块只切换
//! 模式状态，无任何其它执行副作用。

use serde::Serialize;

/// 深诊档资源预算：单核 CPU 占比上限（%）。
pub const DIAG_BUDGET_CPU_PERCENT_SINGLE_CORE: f64 = 2.0;
/// 深诊档资源预算：RSS 上限（MiB）。
pub const DIAG_BUDGET_RSS_MIB: f64 = 64.0;
/// 默认诊断时长（15 分钟）。
pub const DIAG_DEFAULT_DURATION_MS: i64 = 15 * 60_000;
/// 硬上限：单次诊断最长 30 分钟（请求超时一律钳制到该值）。
pub const DIAG_HARD_MAX_DURATION_MS: i64 = 30 * 60_000;

/// 运行模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum DiagState {
    /// 基础模式（默认；40 项采集器裁定不受影响，仅粒度/频率档位不同）。
    Basic,
    /// 诊断模式（有界、到时自动回 Basic）。
    Diagnostic,
}

/// 模式档位参数：采样间隔与 ETW 会话缓冲（深诊档在资源预算内收紧/加大）。
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct ModeProfile {
    /// 通用采样间隔（毫秒）。
    pub sampling_interval_ms: u64,
    /// ETW 会话内核缓冲（KB）。
    pub etw_buffer_kb: u32,
}

/// Basic 档位（生产默认）。
pub const BASIC_PROFILE: ModeProfile = ModeProfile {
    sampling_interval_ms: 30_000,
    etw_buffer_kb: 64,
};
/// 深诊档位（临时；预算 ≤2% 单核 / ≤64MiB 内）。
pub const DIAG_PROFILE: ModeProfile = ModeProfile {
    sampling_interval_ms: 5_000,
    etw_buffer_kb: 256,
};

/// 诊断模式控制器（状态机 + 到期自动收口 + 预算强制回退）。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DiagnosticController {
    pub state: DiagState,
    /// 本次诊断开始时刻（Basic 态为 None）。
    pub started_at_ms: Option<i64>,
    /// 到期时刻（Basic 态为 None）。
    pub expires_at_ms: Option<i64>,
    /// 预算强制回退原因（Basic 态保留最近一次原因供设置页展示；None=无）。
    pub last_forced_reason: Option<String>,
}

impl Default for DiagnosticController {
    fn default() -> Self {
        Self::new()
    }
}

impl DiagnosticController {
    /// 默认：Basic（诊断模式默认关）。
    pub fn new() -> Self {
        Self {
            state: DiagState::Basic,
            started_at_ms: None,
            expires_at_ms: None,
            last_forced_reason: None,
        }
    }

    /// 开启诊断：Basic → Diagnostic，返回实际生效时长（钳制后）。
    /// 已在 Diagnostic 态则幂等刷新到期时刻（同样钳制）。
    pub fn enable(&mut self, now_ms: i64, requested_duration_ms: i64) -> i64 {
        // 硬上限钳制：请求超 30 分钟一律收口到上限（有界铁律）
        let dur = requested_duration_ms.clamp(1, DIAG_HARD_MAX_DURATION_MS);
        self.state = DiagState::Diagnostic;
        self.started_at_ms = Some(now_ms);
        self.expires_at_ms = Some(now_ms + dur);
        dur
    }

    /// 手动提前结束：Diagnostic → Basic（Basic 态幂等）。
    pub fn disable(&mut self, _now_ms: i64) {
        self.state = DiagState::Basic;
        self.started_at_ms = None;
        self.expires_at_ms = None;
    }

    /// 周期巡检：到期自动回 Basic；预算超限（rss_mib/cpu_percent 任一超
    /// 深诊档上限）立即强制回 Basic 并留原因。返回当前态。
    pub fn poll(&mut self, now_ms: i64, rss_mib: f64, cpu_percent_single_core: f64) -> DiagState {
        if self.state != DiagState::Diagnostic {
            return self.state;
        }
        // 预算硬上限：任一超限立即强制回 Basic 并留原因（≥ 上限即视为超，
        // 测试口径：恰好等于预算值不回退，严格大于才回退）
        if rss_mib > DIAG_BUDGET_RSS_MIB {
            let reason = format!(
                "深诊档 RSS {:.1}MiB 超预算上限 {:.0}MiB，强制回 Basic",
                rss_mib, DIAG_BUDGET_RSS_MIB
            );
            self.disable(now_ms);
            self.last_forced_reason = Some(reason);
            return DiagState::Basic;
        }
        if cpu_percent_single_core > DIAG_BUDGET_CPU_PERCENT_SINGLE_CORE {
            let reason = format!(
                "深诊档 CPU {:.2}% 超预算上限 {:.0}%（单核），强制回 Basic",
                cpu_percent_single_core, DIAG_BUDGET_CPU_PERCENT_SINGLE_CORE
            );
            self.disable(now_ms);
            self.last_forced_reason = Some(reason);
            return DiagState::Basic;
        }
        // 到期自动结束（无人干预）
        if let Some(exp) = self.expires_at_ms {
            if now_ms >= exp {
                self.disable(now_ms);
                return DiagState::Basic;
            }
        }
        DiagState::Diagnostic
    }

    /// 剩余时长（毫秒；Basic 态 0；过期未 poll 时按 0 计）。
    pub fn remaining_ms(&self, now_ms: i64) -> i64 {
        match (self.state, self.expires_at_ms) {
            (DiagState::Diagnostic, Some(exp)) => (exp - now_ms).max(0),
            _ => 0,
        }
    }

    /// 当前档位。
    pub fn profile(&self) -> ModeProfile {
        match self.state {
            DiagState::Basic => BASIC_PROFILE,
            DiagState::Diagnostic => DIAG_PROFILE,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: i64 = 1_800_000_000_000;

    #[test]
    fn m42_default_off_basic() {
        let c = DiagnosticController::new();
        assert_eq!(c.state, DiagState::Basic, "诊断模式必须默认关");
        assert_eq!(c.remaining_ms(T0), 0);
        assert_eq!(c.profile(), BASIC_PROFILE);
    }

    #[test]
    fn m42_enable_enters_diagnostic_with_expiry() {
        let mut c = DiagnosticController::new();
        let dur = c.enable(T0, DIAG_DEFAULT_DURATION_MS);
        assert_eq!(dur, DIAG_DEFAULT_DURATION_MS);
        assert_eq!(c.state, DiagState::Diagnostic);
        assert_eq!(c.started_at_ms, Some(T0));
        assert_eq!(c.expires_at_ms, Some(T0 + DIAG_DEFAULT_DURATION_MS));
        assert_eq!(
            c.remaining_ms(T0 + 60_000),
            DIAG_DEFAULT_DURATION_MS - 60_000
        );
        // 深诊档位：采样更密、缓冲更大
        assert!(c.profile().sampling_interval_ms < BASIC_PROFILE.sampling_interval_ms);
        assert!(c.profile().etw_buffer_kb > BASIC_PROFILE.etw_buffer_kb);
    }

    #[test]
    fn m42_hard_deadline_clamps_over_request() {
        let mut c = DiagnosticController::new();
        let dur = c.enable(T0, 6 * 60 * 60_000); // 请求 6 小时
        assert_eq!(dur, DIAG_HARD_MAX_DURATION_MS, "超硬上限必须钳制到 30 分钟");
        assert_eq!(c.expires_at_ms, Some(T0 + DIAG_HARD_MAX_DURATION_MS));
    }

    #[test]
    fn m42_auto_reverts_to_basic_after_expiry() {
        let mut c = DiagnosticController::new();
        c.enable(T0, DIAG_DEFAULT_DURATION_MS);
        // 到期前一刻仍是 Diagnostic
        assert_eq!(
            c.poll(T0 + DIAG_DEFAULT_DURATION_MS - 1, 1.0, 1.0),
            DiagState::Diagnostic
        );
        // 到期自动回 Basic（无人干预）
        assert_eq!(
            c.poll(T0 + DIAG_DEFAULT_DURATION_MS, 1.0, 1.0),
            DiagState::Basic
        );
        assert_eq!(c.state, DiagState::Basic);
        assert_eq!(c.remaining_ms(T0 + DIAG_DEFAULT_DURATION_MS), 0);
        assert_eq!(c.profile(), BASIC_PROFILE, "回 Basic 档位同步恢复");
        // 自动回退后可再次开启（状态机 Basic→Diagnostic→Basic→Diagnostic）
        let dur2 = c.enable(
            T0 + DIAG_DEFAULT_DURATION_MS + 1000,
            DIAG_DEFAULT_DURATION_MS,
        );
        assert_eq!(dur2, DIAG_DEFAULT_DURATION_MS);
        assert_eq!(c.state, DiagState::Diagnostic);
    }

    #[test]
    fn m42_manual_disable_early_and_idempotent() {
        let mut c = DiagnosticController::new();
        c.enable(T0, DIAG_DEFAULT_DURATION_MS);
        c.disable(T0 + 60_000);
        assert_eq!(c.state, DiagState::Basic);
        assert_eq!(c.started_at_ms, None);
        assert_eq!(c.expires_at_ms, None);
        // Basic 上重复 disable 幂等
        c.disable(T0 + 61_000);
        assert_eq!(c.state, DiagState::Basic);
    }

    #[test]
    fn m42_budget_exceeded_forces_basic_with_reason() {
        let mut c = DiagnosticController::new();
        c.enable(T0, DIAG_DEFAULT_DURATION_MS);
        // RSS 超 64MiB 上限 → 立即强制回 Basic
        assert_eq!(
            c.poll(T0 + 30_000, DIAG_BUDGET_RSS_MIB + 1.0, 1.0),
            DiagState::Basic,
            "RSS 超预算必须强制回退"
        );
        let reason = c.last_forced_reason.clone().expect("强制回退必须留原因");
        assert!(reason.contains("RSS"), "{reason}");
        // CPU 超 2% 单核同理
        c.enable(T0, DIAG_DEFAULT_DURATION_MS);
        assert_eq!(
            c.poll(T0 + 30_000, 10.0, DIAG_BUDGET_CPU_PERCENT_SINGLE_CORE + 0.5),
            DiagState::Basic,
            "CPU 超预算必须强制回退"
        );
        assert!(c.last_forced_reason.unwrap().contains("CPU"));
    }

    #[test]
    fn m42_budget_caps_and_within_budget_stays_diagnostic() {
        // 上限断言（硬预算契约）
        assert_eq!(DIAG_BUDGET_CPU_PERCENT_SINGLE_CORE, 2.0);
        assert_eq!(DIAG_BUDGET_RSS_MIB, 64.0);
        const _: () = assert!(DIAG_HARD_MAX_DURATION_MS >= DIAG_DEFAULT_DURATION_MS);
        // 预算内不误杀
        let mut c = DiagnosticController::new();
        c.enable(T0, DIAG_DEFAULT_DURATION_MS);
        assert_eq!(
            c.poll(
                T0 + 30_000,
                DIAG_BUDGET_RSS_MIB,
                DIAG_BUDGET_CPU_PERCENT_SINGLE_CORE
            ),
            DiagState::Diagnostic,
            "恰好达到预算上限（未超过）不触发回退"
        );
    }
}
