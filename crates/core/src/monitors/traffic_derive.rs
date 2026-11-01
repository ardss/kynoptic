//! M3.2 四类流量派生（应用直连/代理相关/代理外层/口径模糊）。
//!
//! 定位：M2 分钟流记录（[`NetFlowRecord`]，含远端地址/端口）+ M3.1 代理
//! 证据（[`super::proxy_link`]）→ **派生分类**指标（source=Derived）。
//!
//! 四类口径（各自独立 metric_id，**绝不求和成单一总量**；方向作 id 后缀
//! 保持分离）：
//! - `net.derived.direct_bytes_{in|out}`：直连流量（非代理进程的非环回流 +
//!   无代理端口证据时不做代理归因的环回……见 classify）——非代理进程的
//!   非环回流量（TUN 未激活时 Confirmed）；
//! - `net.derived.via_proxy_bytes_{in|out}`：代理相关流量（目标=127.0.0.1
//!   代理端口，**有 M3.1 端口证据时** Confirmed）；
//! - `net.derived.proxy_outer_bytes_{in|out}`：代理外层流量（代理核心进程
//!   的对外流量，Confirmed）；
//! - `net.derived.fuzzy_bytes_{in|out}`：口径模糊流量——TUN/多路复用/共享
//!   出口无法精确分配（Estimated）或代理端口证据缺失致环回目标不可判定
//!   （Incomplete）。**每条模糊点带原因（[`DeriveNote`]），禁止硬分摊**。
//!
//! 判定顺序（每条流记录）：代理进程的对外流 → 代理外层；代理进程的环回流
//! → 模糊（用途不可判定）；非代理进程的环回流命中代理端口（有证据）→ 代理
//! 相关；环回但无端口证据 → 模糊（Incomplete）；其余非环回 → 直连（TUN
//! 激活时降模糊 Estimated，原因注入）。

use super::net_etw::{NetDirection, NetFlowRecord, NetProto};
use crate::metrics::{MetricPoint, MetricQuality};
use std::collections::{BTreeMap, HashSet};

/// 直连流量 id 基名（方向后缀 `.bytes_in` / `.bytes_out`）。
pub const METRIC_DERIVED_DIRECT: &str = "net.derived.direct";
/// 代理相关流量 id 基名（应用 → 127.0.0.1 代理端口）。
pub const METRIC_DERIVED_VIA_PROXY: &str = "net.derived.via_proxy";
/// 代理外层流量 id 基名（代理核心进程 → 对外）。
pub const METRIC_DERIVED_PROXY_OUTER: &str = "net.derived.proxy_outer";
/// 口径模糊流量 id 基名（TUN 模糊/证据缺失；带原因，不硬分摊）。
pub const METRIC_DERIVED_FUZZY: &str = "net.derived.fuzzy";

/// 派生分类。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum TrafficClass {
    /// 直连（含环回但非代理端口的本地应用流量）。
    Direct,
    /// 代理相关（环回命中 M3.1 代理端口证据）。
    ViaProxy,
    /// 代理外层（代理核心进程的对外流量）。
    ProxyOuter,
    /// 口径模糊（TUN/证据缺失；Estimated 或 Incomplete + 原因）。
    Fuzzy,
}

impl TrafficClass {
    pub fn metric_base(self) -> &'static str {
        match self {
            TrafficClass::Direct => METRIC_DERIVED_DIRECT,
            TrafficClass::ViaProxy => METRIC_DERIVED_VIA_PROXY,
            TrafficClass::ProxyOuter => METRIC_DERIVED_PROXY_OUTER,
            TrafficClass::Fuzzy => METRIC_DERIVED_FUZZY,
        }
    }

    pub fn metric_id(self, direction: NetDirection) -> String {
        let suffix = match direction {
            NetDirection::In => "bytes_in",
            NetDirection::Out => "bytes_out",
        };
        format!("{}_{}", self.metric_base(), suffix)
    }
}

/// 派生输入上下文（M3.1 证据 + TUN 状态；全部由调用方注入，禁真实时钟）。
#[derive(Debug, Clone, Default)]
pub struct DeriveContext {
    /// M3.1 代理监听端口证据：(端口, 协议)。
    pub proxy_ports: HashSet<(u16, NetProto)>,
    /// M3.1 代理核心进程 pid 集。
    pub proxy_pids: HashSet<u32>,
    /// M3.1 端口枚举是否可用（不可用时环回目标无法判定代理归属）。
    pub port_evidence_available: bool,
    /// TUN/多路复用/共享出口激活原因（Some=激活；直连口径随之降级模糊）。
    pub tun_reason: Option<String>,
}

/// 模糊/降级原因注记（MetricPoint 无备注字段，原因随输出结构显式落账）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeriveNote {
    pub metric_id: String,
    pub minute_bucket: i64,
    pub reason: String,
    /// 该注记对应类的点级 quality（Estimated=口径可辨但混；Incomplete=证据缺失）。
    pub quality: MetricQuality,
}

/// 派生输出：分钟桶 MetricPoint（source=Derived）+ 降级原因注记。
#[derive(Debug, Clone, PartialEq)]
pub struct DeriveOutput {
    pub points: Vec<MetricPoint>,
    pub notes: Vec<DeriveNote>,
}

/// 环回判定：v4 127/8；v6 ::1。
pub fn is_loopback(remote: &[u8], ip_version: u8) -> bool {
    match ip_version {
        4 => remote.first() == Some(&127),
        6 => remote.len() == 16 && remote[..15].iter().all(|&b| b == 0) && remote[15] == 1,
        _ => false,
    }
}

/// 单流分类：(类, quality(模糊类才非 Confirmed), 原因(模糊类)）。
/// 非模糊类恒 (类, Confirmed, None)。
pub fn classify(
    r: &NetFlowRecord,
    ctx: &DeriveContext,
) -> (TrafficClass, MetricQuality, Option<String>) {
    let loopback = is_loopback(&r.remote, r.ip_version);
    if ctx.proxy_pids.contains(&r.pid) {
        if loopback {
            // 代理进程自己的环回（自解析/UI 通道）：用途不可判定，不硬归
            return (
                TrafficClass::Fuzzy,
                MetricQuality::Estimated,
                Some("代理进程环回流量（自解析/UI 通道），用途口径不可分".into()),
            );
        }
        return (TrafficClass::ProxyOuter, MetricQuality::Confirmed, None);
    }
    if loopback {
        if !ctx.port_evidence_available {
            // M3.1 端口证据缺失：环回目标是否代理端口无从判定，不硬归
            return (
                TrafficClass::Fuzzy,
                MetricQuality::Incomplete,
                Some("无代理端口证据（M3.1 枚举不可用），环回目标不可判定".into()),
            );
        }
        if ctx.proxy_ports.contains(&(r.remote_port, r.proto)) {
            return (TrafficClass::ViaProxy, MetricQuality::Confirmed, None);
        }
        // 有证据但端口不在代理集：本地应用直连（环回本就不出网）
        return (TrafficClass::Direct, MetricQuality::Confirmed, None);
    }
    // 非代理进程的非环回流：TUN/多路复用/共享出口激活时直连口径不可精确
    // 分摊（可能实际走隧道）——模糊 + Estimated + 原因，禁止硬分摊
    if let Some(reason) = &ctx.tun_reason {
        return (
            TrafficClass::Fuzzy,
            MetricQuality::Estimated,
            Some(reason.clone()),
        );
    }
    (TrafficClass::Direct, MetricQuality::Confirmed, None)
}

/// 派生：流记录 → 分钟桶四类 MetricPoint（source=Derived；单位 Bytes；
/// timestamp=桶起点；interval=60_000）。桶内同类同向字节求和是**类内**聚合，
/// 类间绝不合并。身份归属经 `identity_of`（同 M2 drain 口径，归属失败
/// exe_start=0 不硬归属）。
pub fn derive_traffic_points(
    records: &[NetFlowRecord],
    ctx: &DeriveContext,
    host_id: &str,
    _now_ms: i64,
    identity_of: &mut dyn FnMut(u32, i64) -> Option<(String, u64)>,
) -> DeriveOutput {
    use crate::metrics::{MetricScope, MetricSource, MetricUnit, ProcessIdentity};
    const MINUTE_MS: i64 = super::net_etw::METRIC_MINUTE_MS;
    // (桶, 类, 方向) → 字节；类间独立键，绝不跨类合并
    let mut agg: BTreeMap<(i64, TrafficClass, NetDirection), (u64, u32)> = BTreeMap::new();
    let mut fuzzy_meta: BTreeMap<(i64, TrafficClass), (MetricQuality, String)> = BTreeMap::new();
    for r in records {
        let (class, quality, reason) = classify(r, ctx);
        let bucket = r.timestamp_utc_ms.div_euclid(MINUTE_MS);
        let e = agg
            .entry((bucket, class, r.direction))
            .or_insert((0, r.pid));
        e.0 += r.bytes;
        if class == TrafficClass::Fuzzy {
            if let (Some(q), Some(reason)) = (Some(quality), reason) {
                fuzzy_meta.entry((bucket, class)).or_insert((q, reason));
            }
        }
    }
    let mut points = Vec::new();
    let mut notes = Vec::new();
    for ((bucket, class, direction), (bytes, pid)) in agg {
        let metric_id = class.metric_id(direction);
        let event_epoch_s = bucket * MINUTE_MS / 1000;
        let identity = identity_of(pid, event_epoch_s);
        let (exe_start, quality) = match identity {
            Some((_name, exe_start)) => (exe_start, MetricQuality::Confirmed),
            None => (0, MetricQuality::Incomplete),
        };
        // 模糊类的 quality 以口径判定为准（Estimated/Incomplete + 原因注记），
        // 身份缺失再降不覆盖口径 quality（口径原因优先留账）
        let quality = if class == TrafficClass::Fuzzy {
            fuzzy_meta
                .get(&(bucket, class))
                .map(|(q, _)| *q)
                .unwrap_or(quality)
        } else {
            quality
        };
        let pi = ProcessIdentity::Id {
            pid,
            exe_start_epoch: exe_start,
        };
        points.push(MetricPoint {
            metric_id: metric_id.clone(),
            source: MetricSource::Derived,
            host_id: host_id.to_string(),
            process_identity: pi,
            timestamp_utc_ms: bucket * MINUTE_MS,
            monotonic_ms: 0, // 桶时间语义无单调意义，恒 0（同 M2 口径）
            interval_ms: MINUTE_MS as u64,
            value: bytes as f64,
            unit: MetricUnit::Bytes,
            scope: MetricScope::Process(pi),
            quality,
            loss_count: None, // 派生口径：丢失已在 M2 上游点显式记账，此处不重复
            coverage: None,
            schema_version: crate::metrics::METRIC_SCHEMA_VERSION,
        });
        if class == TrafficClass::Fuzzy {
            if let Some((q, reason)) = fuzzy_meta.get(&(bucket, class)) {
                notes.push(DeriveNote {
                    metric_id,
                    minute_bucket: bucket,
                    reason: reason.clone(),
                    quality: *q,
                });
            }
        }
    }
    DeriveOutput { points, notes }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::{MetricScope, MetricSource};
    use std::collections::BTreeSet;

    fn flow(pid: u32, remote: Vec<u8>, ver: u8, port: u16, out: bool, bytes: u64) -> NetFlowRecord {
        NetFlowRecord {
            timestamp_utc_ms: 1_800_000_000_000,
            pid,
            direction: if out {
                NetDirection::Out
            } else {
                NetDirection::In
            },
            proto: NetProto::Tcp,
            ip_version: ver,
            remote,
            remote_port: port,
            bytes,
        }
    }

    fn v4_loopback() -> Vec<u8> {
        vec![127, 0, 0, 1]
    }
    fn v4_external() -> Vec<u8> {
        vec![93, 184, 216, 34]
    }

    fn ctx_with_evidence() -> DeriveContext {
        DeriveContext {
            proxy_ports: [(7890u16, NetProto::Tcp)].into_iter().collect(),
            proxy_pids: [4242u32].into_iter().collect(),
            port_evidence_available: true,
            tun_reason: None,
        }
    }

    fn known_identity(_pid: u32, _t: i64) -> Option<(String, u64)> {
        Some(("proc.exe".into(), 1_700_000_000))
    }

    fn ids_of(out: &DeriveOutput) -> BTreeSet<String> {
        out.points.iter().map(|p| p.metric_id.clone()).collect()
    }

    #[test]
    fn m32_loopback_to_proxy_port_hits_via_proxy_confirmed() {
        let ctx = ctx_with_evidence();
        let r = flow(100, v4_loopback(), 4, 7890, true, 5000);
        let (cls, q, reason) = classify(&r, &ctx);
        assert_eq!(cls, TrafficClass::ViaProxy);
        assert_eq!(
            q,
            MetricQuality::Confirmed,
            "有端口证据的代理相关流量必Confirmed"
        );
        assert!(reason.is_none());
    }

    #[test]
    fn m32_loopback_to_non_proxy_port_is_direct() {
        let ctx = ctx_with_evidence();
        let r = flow(100, v4_loopback(), 4, 3306, true, 800);
        let (cls, q, reason) = classify(&r, &ctx);
        assert_eq!(cls, TrafficClass::Direct, "环回但端口无代理证据→本地直连");
        assert_eq!(q, MetricQuality::Confirmed);
        assert!(reason.is_none());
    }

    #[test]
    fn m32_proxy_pid_external_is_proxy_outer_confirmed() {
        let ctx = ctx_with_evidence();
        let r = flow(4242, v4_external(), 4, 443, true, 90_000);
        let (cls, q, reason) = classify(&r, &ctx);
        assert_eq!(cls, TrafficClass::ProxyOuter);
        assert_eq!(q, MetricQuality::Confirmed);
        assert!(reason.is_none());
    }

    #[test]
    fn m32_proxy_pid_loopback_is_fuzzy_estimated_with_reason() {
        // 代理进程自己的环回（自解析/UI 通道）用途不可判定：模糊 + Estimated + 原因
        let ctx = ctx_with_evidence();
        let r = flow(4242, v4_loopback(), 4, 53, true, 120);
        let (cls, q, reason) = classify(&r, &ctx);
        assert_eq!(cls, TrafficClass::Fuzzy);
        assert_eq!(q, MetricQuality::Estimated);
        let reason = reason.expect("模糊必须写原因");
        assert!(!reason.is_empty());
    }

    #[test]
    fn m32_tun_active_downgrades_external_direct_to_fuzzy_estimated() {
        // TUN 激活：非代理进程的对外流无法精确分摊（可能实际走隧道），
        // 禁止硬标直连 Confirmed
        let mut ctx = ctx_with_evidence();
        ctx.tun_reason = Some("TUN 模式激活（wintun 适配器），直连/隧道出口不可精确分配".into());
        let r = flow(100, v4_external(), 4, 443, true, 12_345);
        let (cls, q, reason) = classify(&r, &ctx);
        assert_eq!(cls, TrafficClass::Fuzzy, "TUN 下不得硬标直连");
        assert_eq!(q, MetricQuality::Estimated);
        assert!(reason.unwrap().contains("TUN"));
    }

    #[test]
    fn m32_no_port_evidence_loopback_is_fuzzy_incomplete() {
        // M3.1 枚举不可用：环回命中与否无从判定 → 模糊 + Incomplete + 原因
        let ctx = DeriveContext {
            proxy_ports: [].into(),
            proxy_pids: [].into(),
            port_evidence_available: false,
            tun_reason: None,
        };
        let r = flow(100, v4_loopback(), 4, 7890, true, 600);
        let (cls, q, reason) = classify(&r, &ctx);
        assert_eq!(cls, TrafficClass::Fuzzy);
        assert_eq!(q, MetricQuality::Incomplete);
        assert!(reason.unwrap().contains("证据"), "原因须说明证据缺失");
    }

    #[test]
    fn m32_external_non_proxy_direct_confirmed_without_tun() {
        let ctx = ctx_with_evidence();
        let r = flow(100, v4_external(), 4, 443, false, 40_000);
        let (cls, q, reason) = classify(&r, &ctx);
        assert_eq!(cls, TrafficClass::Direct);
        assert_eq!(q, MetricQuality::Confirmed);
        assert!(reason.is_none());
    }

    #[test]
    fn m32_loopback_detection_v4_and_v6() {
        assert!(is_loopback(&[127, 0, 0, 1], 4));
        assert!(is_loopback(&[127, 200, 3, 9], 4), "127/8 全段环回");
        assert!(!is_loopback(&[192, 168, 1, 1], 4));
        let mut v6lo = vec![0u8; 16];
        v6lo[15] = 1; // ::1
        assert!(is_loopback(&v6lo, 6));
        assert!(!is_loopback(&[0u8; 16], 6));
    }

    #[test]
    fn m32_derive_buckets_four_classes_disjoint_never_summed() {
        // 同一分钟桶内四类流量各自独立出点：类间绝不合并、绝不出总量 id
        let ctx = ctx_with_evidence();
        let records = vec![
            flow(100, v4_loopback(), 4, 7890, true, 5000), // via_proxy
            flow(100, v4_loopback(), 4, 7890, false, 3000), // via_proxy (In)
            flow(4242, v4_external(), 4, 443, true, 90_000), // proxy_outer
            flow(100, v4_external(), 4, 443, false, 40_000), // direct
            flow(100, v4_loopback(), 4, 3306, true, 800),  // direct (环回非代理口)
        ];
        let out = derive_traffic_points(
            &records,
            &ctx,
            "host-x",
            1_800_000_060_000,
            &mut known_identity,
        );
        let ids = ids_of(&out);
        // 四类 × 出现方向：独立 id、互不相交
        assert!(ids.contains("net.derived.via_proxy_bytes_out"));
        assert!(ids.contains("net.derived.via_proxy_bytes_in"));
        assert!(ids.contains("net.derived.proxy_outer_bytes_out"));
        assert!(ids.contains("net.derived.direct_bytes_out"));
        assert_eq!(ids.len(), out.points.len(), "一 id 一点，无跨类合并");
        for id in &ids {
            assert!(
                !id.ends_with("_bytes_total") && !id.contains("total"),
                "禁止总量口径 id：{id}"
            );
        }
        // 值分离断言：via_proxy Out=5000 不含 direct 的 800，也不含 In 的 3000
        let get = |id: &str| {
            out.points
                .iter()
                .find(|p| p.metric_id == id)
                .map(|p| p.value)
                .unwrap()
        };
        assert_eq!(get("net.derived.via_proxy_bytes_out"), 5000.0);
        assert_eq!(get("net.derived.via_proxy_bytes_in"), 3000.0);
        assert_eq!(get("net.derived.proxy_outer_bytes_out"), 90_000.0);
        assert_eq!(
            get("net.derived.direct_bytes_out"),
            800.0,
            "环回非代理口=直连类"
        );
        assert_eq!(get("net.derived.direct_bytes_in"), 40_000.0);
        // 全部 Derived、Bytes、Confirmed（无模糊场景时）
        for p in &out.points {
            assert_eq!(p.source, MetricSource::Derived);
            assert_eq!(p.quality, MetricQuality::Confirmed);
            assert!(matches!(p.scope, MetricScope::Process(_)));
        }
        assert!(out.notes.is_empty(), "无模糊场景不出注记");
    }

    #[test]
    fn m32_fuzzy_points_carry_notes_and_estimated_or_incomplete() {
        // TUN 激活：直连口径流量落模糊类 Estimated + 注记；同时代理相关仍 Confirmed
        let mut ctx = ctx_with_evidence();
        ctx.tun_reason = Some("TUN 模式激活".into());
        let records = vec![
            flow(100, v4_external(), 4, 443, true, 12_345), // fuzzy (TUN)
            flow(100, v4_loopback(), 4, 7890, true, 700),   // via_proxy 不受 TUN 影响
        ];
        let out = derive_traffic_points(
            &records,
            &ctx,
            "host-x",
            1_800_000_060_000,
            &mut known_identity,
        );
        let fuzzy = out
            .points
            .iter()
            .find(|p| p.metric_id == "net.derived.fuzzy_bytes_out")
            .expect("TUN 流量必须落模糊类");
        assert_eq!(fuzzy.quality, MetricQuality::Estimated);
        let note = out
            .notes
            .iter()
            .find(|n| n.metric_id == "net.derived.fuzzy_bytes_out")
            .expect("模糊点必须带原因注记");
        assert!(note.reason.contains("TUN"));
        let via = out
            .points
            .iter()
            .find(|p| p.metric_id == "net.derived.via_proxy_bytes_out")
            .unwrap();
        assert_eq!(
            via.quality,
            MetricQuality::Confirmed,
            "代理相关判定不依赖直连口径"
        );
    }

    #[test]
    fn m32_no_evidence_loopback_note_carries_incomplete_reason() {
        let ctx = DeriveContext {
            port_evidence_available: false,
            ..Default::default()
        };
        let records = vec![flow(100, v4_loopback(), 4, 7890, true, 600)];
        let out = derive_traffic_points(
            &records,
            &ctx,
            "host-x",
            1_800_000_060_000,
            &mut known_identity,
        );
        let p = out
            .points
            .iter()
            .find(|p| p.metric_id == "net.derived.fuzzy_bytes_out")
            .expect("证据缺失的环回必须落模糊类而非硬归直连/代理");
        assert_eq!(p.quality, MetricQuality::Incomplete);
        assert!(
            out.notes.iter().any(|n| n.reason.contains("证据")),
            "原因注记必须说明证据缺失"
        );
    }

    #[test]
    fn m32_identity_attached_and_unresolvable_keeps_pid_zero_start() {
        // 身份归属同 M2 口径：可归属带 exe_start；不可归属 pid 保留、exe_start=0
        let ctx = ctx_with_evidence();
        let records = vec![flow(100, v4_loopback(), 4, 7890, true, 500)];
        let mut called = 0;
        let out = derive_traffic_points(&records, &ctx, "h", 0, &mut |pid, _t| {
            called += 1;
            if pid == 100 {
                Some(("app.exe".into(), 1_700_000_000))
            } else {
                None
            }
        });
        assert_eq!(called, 1, "身份解析按桶 pid 调用");
        let p = &out.points[0];
        match &p.process_identity {
            crate::metrics::ProcessIdentity::Id {
                pid,
                exe_start_epoch,
            } => {
                assert_eq!(*pid, 100);
                assert_eq!(*exe_start_epoch, 1_700_000_000);
            }
            other => panic!("期望 Id 身份，实际 {other:?}"),
        }
    }
}
