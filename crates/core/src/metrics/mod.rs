//! M1 统一指标契约层（Kynoptic 可观测性升级 M1）。
//!
//! 定位（复核裁定 2026-11-01）：[`MetricPoint`] 是**内存/序列化契约层**——
//! 12+1 字段统一信封，M2 的 ETW 采集以其为产出类型；带 metric_id 的独立
//! 持久化在 M2 落地（并复用 S1.1 的事务清理骨架）。M1 不建新表、不改
//! /api、不改 dash——纯契约 + agg_minute 桥证明可集成。
//!
//! 字段语义（需求原文 §四）：
//! - 「未采到不是零」：[`MetricQuality::Unavailable`] ≠ value=0，消费方不得
//!   把未采到当零聚合；
//! - 「估算值不是实测值」：[`MetricQuality::Estimated`] ≠ Confirmed。
//! - `timestamp_utc_ms`：UTC epoch 毫秒（i64）；
//! - `monotonic_ms`：本机单调时钟相对值（u64），**仅本机内可比**，不跨进程/
//!   不跨机器比较；
//! - `interval_ms`：实际采样窗口（毫秒）；
//! - `loss_count`/`coverage`：ETW 丢事件观测，仅丢事件时有值（Option）。
//!
//! metric_id 命名规范：`<domain>.<subject>.<measure>` 三段小写点分
//! （如 `cpu.host.cpu_percent`、`net.iface.bytes_sent`）。本批已知 id 以
//! `pub const` 收口（cpu/mem/disk/net 四域整机型+进程型各至少一条）。

use serde::{Deserialize, Serialize};

/// 指标信封 schema 基线（M1 = 1；字段增删必须 bump 并在消费方兼容窗口内）。
pub const METRIC_SCHEMA_VERSION: u32 = 1;

/// 已知指标 id 收口（`<domain>.<subject>.<measure>`，全小写点分；格式由
/// m1_known_metric_ids_are_lowercase_dotted 钉住）。
pub const METRIC_CPU_HOST_CPU_PERCENT: &str = "cpu.host.cpu_percent";
pub const METRIC_CPU_PROCESS_CPU_TIME: &str = "cpu.process.cpu_time";
pub const METRIC_MEM_HOST_COMMITTED: &str = "mem.host.committed_bytes";
pub const METRIC_MEM_PROCESS_WORKING_SET: &str = "mem.process.working_set_bytes";
pub const METRIC_DISK_HOST_IO_READ: &str = "disk.host.io_read_bytes";
pub const METRIC_DISK_PROCESS_IO_READ: &str = "disk.process.io_read_bytes";
pub const METRIC_NET_IFACE_BYTES_SENT: &str = "net.iface.bytes_sent";
pub const METRIC_NET_HOST_BYTES_SENT: &str = "net.host.bytes_sent";

/// 采集来源（防「值从哪来」口径混用：ETW 实时流 ≠ WinAPI 轮询 ≠ 派生计算）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MetricSource {
    /// ETW 实时事件流（M2 接入）。
    Etw,
    /// Win32 API 轮询（现有 monitors 家族）。
    WinApi,
    /// 从已有聚合/缓存派生（如 agg_minute 桥）。
    Derived,
    /// 采集代理核心口径（外部 agent 上报）。
    ProxyCore,
    /// 第三方 provider 直报。
    Provider,
}

/// 进程身份（PID + 启动时间防 PID 复用误绑）；非进程粒度用 [`ProcessIdentity::None`]。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProcessIdentity {
    None,
    Id {
        pid: u32,
        /// 进程镜像启动时刻（UTC epoch 秒）——同 PID 复用后值不同，绑定凭据。
        exe_start_epoch: u64,
    },
}

/// 计量单位（禁止数值自带单位，展示层按 [`MetricUnit::suffix`] 渲染）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MetricUnit {
    Bytes,
    Kib,
    Mib,
    Percent,
    Count,
    Milliseconds,
    BytesPerSec,
}

impl MetricUnit {
    /// 展示层单位后缀（互异、非空，防混用——测试钉住）。
    pub fn suffix(&self) -> &'static str {
        match self {
            MetricUnit::Bytes => "B",
            MetricUnit::Kib => "KiB",
            MetricUnit::Mib => "MiB",
            MetricUnit::Percent => "%",
            MetricUnit::Count => "n",
            MetricUnit::Milliseconds => "ms",
            MetricUnit::BytesPerSec => "B/s",
        }
    }
}

/// 指标作用域（信封与 scope 联动：进程粒度指标 scope 必须携带进程身份）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MetricScope {
    Host,
    Interface { name: String },
    Process(ProcessIdentity),
    Connection { proto: String, remote: String },
    App { name: String },
    Provider { name: String },
}

/// 数据质量四态（需求原文语义：「未采到不是零，估算值不是实测值」）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MetricQuality {
    /// 实测确认值。
    Confirmed,
    /// 估算值（不是实测）。
    Estimated,
    /// 采样窗口不完整（部分缺失）。
    Incomplete,
    /// 未采到（不是零——消费方不得当 0 聚合）。
    Unavailable,
}

impl MetricQuality {
    /// 质量标记（互异，供聚合/展示按质量分桶；测试钉住四态互异）。
    pub fn tag(&self) -> &'static str {
        match self {
            MetricQuality::Confirmed => "confirmed",
            MetricQuality::Estimated => "estimated",
            MetricQuality::Incomplete => "incomplete",
            MetricQuality::Unavailable => "unavailable",
        }
    }
}

/// 统一指标信封（M1 契约，schema_version=1）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MetricPoint {
    /// `<domain>.<subject>.<measure>` 稳定标识（已知 id 见本模块 `METRIC_*` 常量）。
    pub metric_id: String,
    pub source: MetricSource,
    pub host_id: String,
    pub process_identity: ProcessIdentity,
    /// UTC epoch 毫秒。
    pub timestamp_utc_ms: i64,
    /// 本机单调时钟相对毫秒（仅本机内可比）。
    pub monotonic_ms: u64,
    /// 实际采样窗口（毫秒）。
    pub interval_ms: u64,
    pub value: f64,
    pub unit: MetricUnit,
    pub scope: MetricScope,
    pub quality: MetricQuality,
    /// ETW 丢事件计数（仅丢事件时有值）。
    pub loss_count: Option<u64>,
    /// 采样覆盖率 0.0-1.0（仅不完整窗口时有值）。
    pub coverage: Option<f64>,
    pub schema_version: u32,
}

impl MetricPoint {
    /// agg_minute 一行 → MetricPoint 桥（M1「证明可集成」的交付件）。
    ///
    /// agg_minute（db/migrations/0002_bucket_model.sql）是输入聚合缓存：
    /// bucket_id（如 `input_keys`）+ 分钟粒度 sum_value/count_value。桥按
    /// 命名规范派生 `input.host.<bucket_id>`——**裁定偏差说明**：裁定示例
    /// 写的是 cpu.host.cpu_percent，但 agg_minute 数据域是输入聚合（不是
    /// cpu），照搬会把 cpu 标签贴在输入数据上违反 `<domain>` 真实性规范，
    /// 故按同规范类推 input.host.*（cpu.* 等 id 留给 M2 真实 cpu 采集）。
    /// source=Derived（缓存行）、quality=Confirmed（实测确认——「估算不是
    /// 实测」，缓存行是实测）、unit=Count（bucket 计数口径）。
    #[allow(clippy::too_many_arguments)]
    pub fn from_agg_minute_row(
        bucket_id: &str,
        sum_value: f64,
        _count_value: i64,
        interval_ms: u64,
        host_id: &str,
        timestamp_utc_ms: i64,
        monotonic_ms: u64,
    ) -> Self {
        MetricPoint {
            metric_id: format!("input.host.{bucket_id}"),
            source: MetricSource::Derived,
            host_id: host_id.to_string(),
            process_identity: ProcessIdentity::None,
            timestamp_utc_ms,
            monotonic_ms,
            interval_ms,
            value: sum_value,
            unit: MetricUnit::Count,
            scope: MetricScope::Host,
            quality: MetricQuality::Confirmed,
            loss_count: None,
            coverage: None,
            schema_version: METRIC_SCHEMA_VERSION,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn m1_metric_point_serde_round_trip_full_fields() {
        let p = MetricPoint {
            metric_id: METRIC_CPU_HOST_CPU_PERCENT.to_string(),
            source: MetricSource::Derived,
            host_id: "HOST-1".to_string(),
            process_identity: ProcessIdentity::None,
            timestamp_utc_ms: 1_749_000_000_000,
            monotonic_ms: 123_456,
            interval_ms: 30_000,
            value: 42.5,
            unit: MetricUnit::Percent,
            scope: MetricScope::Host,
            quality: MetricQuality::Confirmed,
            loss_count: None,
            coverage: None,
            schema_version: METRIC_SCHEMA_VERSION,
        };
        let s = serde_json::to_string(&p).expect("序列化不得失败");
        let back: MetricPoint = serde_json::from_str(&s).expect("反序列化不得失败");
        assert_eq!(back.metric_id, METRIC_CPU_HOST_CPU_PERCENT);
        assert!(matches!(back.source, MetricSource::Derived));
        assert_eq!(back.host_id, "HOST-1");
        assert!(matches!(back.process_identity, ProcessIdentity::None));
        assert_eq!(back.timestamp_utc_ms, 1_749_000_000_000);
        assert_eq!(back.monotonic_ms, 123_456);
        assert_eq!(back.interval_ms, 30_000);
        assert_eq!(back.value, 42.5);
        assert!(matches!(back.unit, MetricUnit::Percent));
        assert!(matches!(back.scope, MetricScope::Host));
        assert!(matches!(back.quality, MetricQuality::Confirmed));
        assert_eq!(back.loss_count, None);
        assert_eq!(back.coverage, None);
        assert_eq!(back.schema_version, METRIC_SCHEMA_VERSION);
    }

    #[test]
    fn m1_process_identity_carries_pid_and_exe_start() {
        let p = MetricPoint {
            metric_id: METRIC_CPU_PROCESS_CPU_TIME.to_string(),
            source: MetricSource::WinApi,
            host_id: "HOST-1".to_string(),
            process_identity: ProcessIdentity::Id {
                pid: 4242,
                exe_start_epoch: 1_749_000_000,
            },
            timestamp_utc_ms: 1,
            monotonic_ms: 0,
            interval_ms: 1_000,
            value: 1.0,
            unit: MetricUnit::Milliseconds,
            scope: MetricScope::Process(ProcessIdentity::Id {
                pid: 4242,
                exe_start_epoch: 1_749_000_000,
            }),
            quality: MetricQuality::Confirmed,
            loss_count: Some(3),
            coverage: Some(0.97),
            schema_version: METRIC_SCHEMA_VERSION,
        };
        let s = serde_json::to_string(&p).unwrap();
        assert!(s.contains("\"pid\":4242"));
        assert!(s.contains("\"exe_start_epoch\":1749000000"));
        let back: MetricPoint = serde_json::from_str(&s).unwrap();
        match back.scope {
            MetricScope::Process(ProcessIdentity::Id { pid, .. }) => assert_eq!(pid, 4242),
            other => panic!("scope 必须是进程粒度，实际 {other:?}"),
        }
        assert_eq!(back.loss_count, Some(3));
        assert_eq!(back.coverage, Some(0.97));
    }

    #[test]
    fn m1_unit_suffixes_are_display_ready_and_distinct() {
        // 防混用防线：各单位后缀互异且非空（展示层按 unit 渲染，禁止数值自带单位）
        let units = [
            MetricUnit::Bytes,
            MetricUnit::Kib,
            MetricUnit::Mib,
            MetricUnit::Percent,
            MetricUnit::Count,
            MetricUnit::Milliseconds,
            MetricUnit::BytesPerSec,
        ];
        let mut suffixes: Vec<String> = units.iter().map(|u| u.suffix().to_string()).collect();
        assert!(suffixes.iter().all(|s| !s.is_empty()), "单位后缀必须非空");
        suffixes.sort();
        let n = suffixes.len();
        suffixes.dedup();
        assert_eq!(suffixes.len(), n, "各单位后缀必须互异（防展示混淆）");
    }

    #[test]
    fn m1_quality_semantics_anchor() {
        // 需求原文语义锚：「未采到不是零，估算值不是实测值」——四态必须互异
        // 且 Unavailable ≠ 0（消费方不得把 Unavailable 当 0 聚合）
        let qs = [
            MetricQuality::Confirmed,
            MetricQuality::Estimated,
            MetricQuality::Incomplete,
            MetricQuality::Unavailable,
        ];
        let mut tags: Vec<String> = qs.iter().map(|q| q.tag().to_string()).collect();
        tags.sort();
        tags.dedup();
        assert_eq!(tags.len(), 4, "质量四态标记必须互异");
    }

    #[test]
    fn m1_schema_version_is_one_by_contract() {
        assert_eq!(METRIC_SCHEMA_VERSION, 1, "M1 契约基线 schema_version=1");
    }

    #[test]
    fn m1_known_metric_ids_are_lowercase_dotted() {
        for id in [
            METRIC_CPU_HOST_CPU_PERCENT,
            METRIC_CPU_PROCESS_CPU_TIME,
            METRIC_MEM_HOST_COMMITTED,
            METRIC_MEM_PROCESS_WORKING_SET,
            METRIC_DISK_HOST_IO_READ,
            METRIC_DISK_PROCESS_IO_READ,
            METRIC_NET_IFACE_BYTES_SENT,
            METRIC_NET_HOST_BYTES_SENT,
        ] {
            assert!(!id.is_empty(), "指标 id 不得为空");
            assert_eq!(id, id.to_lowercase(), "指标 id 必须全小写: {id}");
            assert!(
                id.chars()
                    .all(|c| c.is_ascii_lowercase() || c == '.' || c == '_'),
                "指标 id 只允许小写字母/点/下划线: {id}"
            );
            assert_eq!(
                id.split('.').count(),
                3,
                "指标 id 必须 <domain>.<subject>.<measure> 三段: {id}"
            );
        }
    }

    #[test]
    fn m1_bridge_from_agg_minute_row_sets_derived_confirmed() {
        // agg_minute 一行（输入聚合缓存）→ MetricPoint（Derived/Confirmed）
        let p = MetricPoint::from_agg_minute_row(
            "input_keys",
            1234.0,
            567,
            60_000,
            "HOST-1",
            1_749_000_000_000,
            456_789,
        );
        assert_eq!(p.metric_id, "input.host.input_keys");
        assert!(
            matches!(p.source, MetricSource::Derived),
            "聚合缓存是派生数据"
        );
        assert!(
            matches!(p.quality, MetricQuality::Confirmed),
            "缓存行是实测确认值（未采到不是零、估算不是实测——缓存行属实测）"
        );
        assert_eq!(p.value, 1234.0);
        assert_eq!(p.interval_ms, 60_000);
        assert_eq!(p.host_id, "HOST-1");
        assert_eq!(p.schema_version, METRIC_SCHEMA_VERSION);
        assert!(matches!(p.unit, MetricUnit::Count));
    }
}
