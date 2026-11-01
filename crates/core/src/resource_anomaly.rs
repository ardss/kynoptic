//! M4.1 统一资源异常模型 ResourceAnomaly 与检测器集合。
//!
//! 定位：**只检测与记录**——产出统一异常事件（metric_id、起止时刻、
//! process_identity、证据数字、滚动基线、severity、confidence、关联事件
//! id），不自动结束程序、不改代理配置、不阻断网络（无任何执行副作用）。
//!
//! 防误报口径：滚动基线（近 7 天同小时分位，[`hour_baseline`]）+ 可配置
//! 绝对阈值**组合**判定——有基线时须同时超 `p95 × multiplier` 与绝对阈值；
//! 无基线（样本不足）仅绝对阈值，confidence 相应下调。
//!
//! 持久化复用：[`ResourceAnomaly::to_anomaly`] 把统一事件折算进现有
//! [`crate::anomaly::Anomaly`] 展示/消费面（dash /api/anomalies、MCP
//! get_anomalies 同管道），并携带 schema_version（本切片扩展字段）。

use serde::Serialize;

/// 资源异常事件 schema 版本（M4.1 首版）。
pub const RESOURCE_ANOMALY_SCHEMA_VERSION: u32 = 1;

/// 轻量进程身份（归属失败不伪造：None）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProcessIdentityLite {
    pub pid: u32,
    pub name: String,
    pub exe_start_epoch: u64,
}

/// 证据数字（label 数值化，不做自然语言渲染）。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Evidence {
    pub label: String,
    pub value: f64,
    pub unit: String,
}

/// 滚动基线（同小时分位）。
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Baseline {
    pub p50: f64,
    pub p95: f64,
    pub samples: usize,
}

/// 统一资源异常事件。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ResourceAnomaly {
    pub schema_version: u32,
    /// 异常种类（traffic_surge / sustained_upload / cpu_sustained /
    /// cpu_periodic_spike / mem_growth / disk_throughput /
    /// short_lived_process / process_fanout / collector_failure /
    /// network_switch）。
    pub kind: String,
    /// 触发指标（M1 三段 metric_id）。
    pub metric_id: String,
    pub started_at_ms: i64,
    /// 未收口（进行中）为 None。
    pub ended_at_ms: Option<i64>,
    pub process_identity: Option<ProcessIdentityLite>,
    /// 证据数字（实测值、基线值、阈值等）。
    pub evidence: Vec<Evidence>,
    /// "info" | "warn" | "alert"
    pub severity: String,
    /// 0.0..=1.0
    pub confidence: f64,
    /// 命中时的滚动基线（无基线 None）。
    pub baseline: Option<Baseline>,
    /// 关联事件 id（S0.2 告警通道 events 表 rowid 等）。
    pub related_event_ids: Vec<i64>,
    /// 口径/原因备注。
    pub note: String,
}

/// 检测器公共上下文（归属与关联由调用方注入）。
#[derive(Debug, Clone, Default)]
pub struct AnomalyCtx<'a> {
    pub metric_id: &'a str,
    pub identity: Option<ProcessIdentityLite>,
    pub related_event_ids: Vec<i64>,
}

/// 时间序列点（毫秒时刻，值）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SeriesPoint {
    pub t_ms: i64,
    pub v: f64,
}

/// 检测阈值集（全部可配置；默认值见 [`DetectorConfig::default`]）。
#[derive(Debug, Clone)]
pub struct DetectorConfig {
    /// 流量/磁盘：每分钟绝对阈值（字节）。
    pub traffic_abs_bytes_per_min: f64,
    /// 持续上传：连续超基线分钟数。
    pub sustained_upload_minutes: u32,
    /// CPU 持续高：窗口均值绝对阈值（%）。
    pub cpu_abs_percent: f64,
    /// CPU 持续高：须持续分钟数。
    pub cpu_sustained_minutes: u32,
    /// CPU 周期尖峰：最少尖峰次数。
    pub cpu_periodic_min_spikes: u32,
    /// CPU 尖峰判定值（%）。
    pub cpu_spike_percent: f64,
    /// 内存增长斜率阈值（MB/小时）。
    pub mem_growth_mb_per_hour: f64,
    /// 磁盘高吞吐：每分钟绝对阈值（字节）。
    pub disk_abs_bytes_per_min: f64,
    /// 短命进程：寿命上限（秒）。
    pub short_lived_max_lifetime_s: u64,
    /// 短命进程：同窗口最少世数。
    pub short_lived_min_count: u32,
    /// 单父多子：最少子进程数。
    pub fanout_min_children: u32,
    /// 基线触发倍数（p95 × multiplier）。
    pub baseline_multiplier: f64,
    /// 基线最少样本数（不足则视为无基线）。
    pub baseline_min_samples: usize,
}

impl Default for DetectorConfig {
    fn default() -> Self {
        Self {
            traffic_abs_bytes_per_min: 50.0 * 1024.0 * 1024.0,
            sustained_upload_minutes: 10,
            cpu_abs_percent: 85.0,
            cpu_sustained_minutes: 10,
            cpu_periodic_min_spikes: 3,
            cpu_spike_percent: 95.0,
            mem_growth_mb_per_hour: 200.0,
            disk_abs_bytes_per_min: 500.0 * 1024.0 * 1024.0,
            short_lived_max_lifetime_s: 5,
            short_lived_min_count: 5,
            fanout_min_children: 10,
            baseline_multiplier: 4.0,
            baseline_min_samples: 10,
        }
    }
}

/// 分位（线性插值；输入须升序；空输入返回 0）。
pub fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let n = sorted.len();
    let idx = p.clamp(0.0, 1.0) * (n - 1) as f64;
    let lo = idx.floor() as usize;
    let hi = idx.ceil() as usize;
    if lo == hi {
        return sorted[lo];
    }
    sorted[lo] + (sorted[hi] - sorted[lo]) * (idx - lo as f64)
}

/// 近 7 天同小时滚动基线：`history` 为 (小时 0-23, 值) 对（近 7 天逐点），
/// 取同小时样本；样本数 < `min_samples` → None（样本不足不设基线）。
pub fn hour_baseline(history: &[(u32, f64)], hour: u32, min_samples: usize) -> Option<Baseline> {
    let mut vals: Vec<f64> = history
        .iter()
        .filter(|(h, _)| *h == hour)
        .map(|(_, v)| *v)
        .collect();
    if min_samples == 0 || vals.len() < min_samples {
        return None;
    }
    vals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    Some(Baseline {
        p50: percentile(&vals, 0.5),
        p95: percentile(&vals, 0.95),
        samples: vals.len(),
    })
}

/// 基线+绝对阈值组合判定（防误报核心）：
/// - 有基线：`v > p95 * multiplier && v > abs` 才命中；
/// - 无基线：仅 `v > abs`（confidence 下调 0.6）。
///
/// 命中返回 (confidence, 基线快照)。
pub fn baseline_abs_gate(
    v: f64,
    baseline: Option<Baseline>,
    abs: f64,
    cfg: &DetectorConfig,
) -> Option<(f64, Option<Baseline>)> {
    if v <= abs {
        return None;
    }
    match baseline {
        // 防误报：有基线必须同时超 p95×multiplier 与绝对阈值
        Some(b) if v > b.p95 * cfg.baseline_multiplier => Some((0.95, Some(b))),
        Some(_) => None,
        None => Some((0.6, None)), // 无基线仅绝对门，confidence 下调
    }
}

/// 流量突增：窗口内任一分钟超组合门。
pub fn detect_traffic_surge(
    series: &[SeriesPoint],
    baseline: Option<Baseline>,
    cfg: &DetectorConfig,
    ctx: &AnomalyCtx,
) -> Vec<ResourceAnomaly> {
    let _ = (series, baseline, cfg, ctx);
    let mut out = Vec::new();
    for p in series {
        if let Some((conf, bl)) =
            baseline_abs_gate(p.v, baseline, cfg.traffic_abs_bytes_per_min, cfg)
        {
            out.push(ResourceAnomaly {
                schema_version: RESOURCE_ANOMALY_SCHEMA_VERSION,
                kind: "traffic_surge".into(),
                metric_id: ctx.metric_id.to_string(),
                started_at_ms: p.t_ms,
                ended_at_ms: None,
                process_identity: ctx.identity.clone(),
                evidence: vec![
                    Evidence {
                        label: "value_bytes_per_min".into(),
                        value: p.v,
                        unit: "B/min".into(),
                    },
                    Evidence {
                        label: "abs_threshold_bytes_per_min".into(),
                        value: cfg.traffic_abs_bytes_per_min,
                        unit: "B/min".into(),
                    },
                ],
                severity: "warn".into(),
                confidence: conf,
                baseline: bl,
                related_event_ids: ctx.related_event_ids.clone(),
                note: "分钟字节量超滚动基线 p95×倍数 与绝对阈值的组合门（防误报）".into(),
            });
        }
    }
    out
}

/// 持续上传：连续 ≥ N 分钟每分钟都过组合门（方向性出口流量）。
pub fn detect_sustained_upload(
    series: &[SeriesPoint],
    baseline: Option<Baseline>,
    cfg: &DetectorConfig,
    ctx: &AnomalyCtx,
) -> Vec<ResourceAnomaly> {
    let _ = (series, baseline, cfg, ctx);
    let need = cfg.sustained_upload_minutes.max(1) as usize;
    let mut run_start: Option<i64> = None;
    let mut run_len = 0usize;
    let mut run_peak = 0.0f64;
    let mut out = Vec::new();
    for p in series {
        if baseline_abs_gate(p.v, baseline, cfg.traffic_abs_bytes_per_min, cfg).is_some() {
            if run_start.is_none() {
                run_start = Some(p.t_ms);
                run_peak = p.v;
            }
            run_peak = run_peak.max(p.v);
            run_len += 1;
            if run_len == need {
                out.push(ResourceAnomaly {
                    schema_version: RESOURCE_ANOMALY_SCHEMA_VERSION,
                    kind: "sustained_upload".into(),
                    metric_id: ctx.metric_id.to_string(),
                    started_at_ms: run_start.unwrap(),
                    ended_at_ms: None,
                    process_identity: ctx.identity.clone(),
                    evidence: vec![
                        Evidence {
                            label: "consecutive_minutes".into(),
                            value: need as f64,
                            unit: "min".into(),
                        },
                        Evidence {
                            label: "peak_bytes_per_min".into(),
                            value: run_peak,
                            unit: "B/min".into(),
                        },
                    ],
                    severity: "alert".into(),
                    confidence: 0.85,
                    baseline,
                    related_event_ids: ctx.related_event_ids.clone(),
                    note: "出口流量连续多分钟超组合门（持续上传特征）".into(),
                });
                return out;
            }
        } else {
            run_start = None;
            run_len = 0;
            run_peak = 0.0;
        }
    }
    out
}

/// CPU 持续高：连续 ≥ N 分钟均值 ≥ cpu_abs_percent。
pub fn detect_cpu_sustained(
    series: &[SeriesPoint],
    cfg: &DetectorConfig,
    ctx: &AnomalyCtx,
) -> Vec<ResourceAnomaly> {
    let _ = (series, cfg, ctx);
    let need = cfg.cpu_sustained_minutes.max(1) as usize;
    let mut run: Vec<&SeriesPoint> = Vec::new();
    for p in series {
        if p.v >= cfg.cpu_abs_percent {
            run.push(p);
        } else {
            run.clear();
            continue;
        }
        if run.len() == need {
            let avg = run.iter().map(|x| x.v).sum::<f64>() / run.len() as f64;
            return vec![ResourceAnomaly {
                schema_version: RESOURCE_ANOMALY_SCHEMA_VERSION,
                kind: "cpu_sustained".into(),
                metric_id: ctx.metric_id.to_string(),
                started_at_ms: run[0].t_ms,
                ended_at_ms: None,
                process_identity: ctx.identity.clone(),
                evidence: vec![
                    Evidence {
                        label: "avg_percent".into(),
                        value: avg,
                        unit: "%".into(),
                    },
                    Evidence {
                        label: "consecutive_minutes".into(),
                        value: need as f64,
                        unit: "min".into(),
                    },
                ],
                severity: if avg >= 95.0 { "alert" } else { "warn" }.into(),
                confidence: 0.9,
                baseline: None,
                related_event_ids: ctx.related_event_ids.clone(),
                note: "CPU 连续多分钟不低于绝对阈值（持续高负载）".into(),
            }];
        }
    }
    Vec::new()
}

/// CPU 周期尖峰：≥ N 次超过 spike 值且尖峰间隔近似等周期（间隔变异系数
/// ≤ 0.25）——定时任务特征；无周期性（随机尖峰）不报。
pub fn detect_cpu_periodic_spike(
    series: &[SeriesPoint],
    cfg: &DetectorConfig,
    ctx: &AnomalyCtx,
) -> Vec<ResourceAnomaly> {
    let _ = (series, cfg, ctx);
    let spikes: Vec<i64> = series
        .iter()
        .filter(|p| p.v >= cfg.cpu_spike_percent)
        .map(|p| p.t_ms)
        .collect();
    if (spikes.len() as u32) < cfg.cpu_periodic_min_spikes {
        return Vec::new();
    }
    // 等周期判定：相邻尖峰间隔的变异系数（stddev/mean）≤ 0.25
    let gaps: Vec<f64> = spikes.windows(2).map(|w| (w[1] - w[0]) as f64).collect();
    if gaps.is_empty() {
        return Vec::new();
    }
    let mean = gaps.iter().sum::<f64>() / gaps.len() as f64;
    let var = gaps.iter().map(|g| (g - mean) * (g - mean)).sum::<f64>() / gaps.len() as f64;
    let cv = if mean > 0.0 {
        var.sqrt() / mean
    } else {
        f64::INFINITY
    };
    if cv > 0.25 {
        return Vec::new(); // 无周期性（随机尖峰）不报——防误报
    }
    vec![ResourceAnomaly {
        schema_version: RESOURCE_ANOMALY_SCHEMA_VERSION,
        kind: "cpu_periodic_spike".into(),
        metric_id: ctx.metric_id.to_string(),
        started_at_ms: spikes[0],
        ended_at_ms: Some(*spikes.last().unwrap()),
        process_identity: ctx.identity.clone(),
        evidence: vec![
            Evidence {
                label: "spike_count".into(),
                value: spikes.len() as f64,
                unit: "次".into(),
            },
            Evidence {
                label: "period_ms".into(),
                value: mean,
                unit: "ms".into(),
            },
            Evidence {
                label: "gap_cv".into(),
                value: cv,
                unit: "".into(),
            },
        ],
        severity: "info".into(),
        confidence: 0.8,
        baseline: None,
        related_event_ids: ctx.related_event_ids.clone(),
        note: "CPU 尖峰近似等周期（定时任务特征；随机尖峰不报）".into(),
    }]
}

/// 内存增长斜率：线性回归斜率 ≥ mem_growth_mb_per_hour（MB/h，序列值单位
/// MB）且跨窗 ≥ 30 分钟。
pub fn detect_mem_growth(
    series: &[SeriesPoint],
    cfg: &DetectorConfig,
    ctx: &AnomalyCtx,
) -> Vec<ResourceAnomaly> {
    let _ = (series, cfg, ctx);
    // 跨窗须 ≥ 30 分钟，否则斜率无意义
    if series.len() < 2 {
        return Vec::new();
    }
    let (t0, t1) = (series.first().unwrap().t_ms, series.last().unwrap().t_ms);
    if t1 - t0 < 30 * 60_000 {
        return Vec::new();
    }
    let n = series.len() as f64;
    let st = series.iter().map(|p| p.t_ms as f64).sum::<f64>();
    let sv = series.iter().map(|p| p.v).sum::<f64>();
    let stt = series
        .iter()
        .map(|p| p.t_ms as f64 * p.t_ms as f64)
        .sum::<f64>();
    let stv = series.iter().map(|p| p.t_ms as f64 * p.v).sum::<f64>();
    let denom = n * stt - st * st;
    if denom == 0.0 {
        return Vec::new();
    }
    // 斜率单位：值/毫秒 → MB/小时
    let slope_mb_per_h = (n * stv - st * sv) / denom * 3_600_000.0;
    if slope_mb_per_h < cfg.mem_growth_mb_per_hour {
        return Vec::new();
    }
    vec![ResourceAnomaly {
        schema_version: RESOURCE_ANOMALY_SCHEMA_VERSION,
        kind: "mem_growth".into(),
        metric_id: ctx.metric_id.to_string(),
        started_at_ms: t0,
        ended_at_ms: Some(t1),
        process_identity: ctx.identity.clone(),
        evidence: vec![
            Evidence {
                label: "slope_mb_per_hour".into(),
                value: slope_mb_per_h,
                unit: "MB/h".into(),
            },
            Evidence {
                label: "window_minutes".into(),
                value: (t1 - t0) as f64 / 60_000.0,
                unit: "min".into(),
            },
        ],
        severity: "warn".into(),
        confidence: 0.8,
        baseline: None,
        related_event_ids: ctx.related_event_ids.clone(),
        note: "内存线性回归斜率超阈值（疑似泄漏式增长；线性拟合不硬归因）".into(),
    }]
}

/// 磁盘高吞吐：任一分钟过绝对门（持续 ≥ 2 分钟才算，排除单点抖动）。
pub fn detect_disk_throughput(
    series: &[SeriesPoint],
    cfg: &DetectorConfig,
    ctx: &AnomalyCtx,
) -> Vec<ResourceAnomaly> {
    // 持续 ≥ 2 分钟才报（排除单点抖动）
    let mut run_len = 0usize;
    let mut run_start = 0i64;
    let mut run_peak = 0.0f64;
    let mut peak_at = 0i64;
    for p in series {
        if p.v > cfg.disk_abs_bytes_per_min {
            if run_len == 0 {
                run_start = p.t_ms;
            }
            if p.v > run_peak {
                run_peak = p.v;
                peak_at = p.t_ms;
            }
            run_len += 1;
        } else {
            run_len = 0;
            run_peak = 0.0;
        }
        if run_len >= 2 {
            return vec![ResourceAnomaly {
                schema_version: RESOURCE_ANOMALY_SCHEMA_VERSION,
                kind: "disk_throughput".into(),
                metric_id: ctx.metric_id.to_string(),
                started_at_ms: run_start,
                ended_at_ms: None,
                process_identity: ctx.identity.clone(),
                evidence: vec![
                    Evidence {
                        label: "peak_bytes_per_min".into(),
                        value: run_peak,
                        unit: "B/min".into(),
                    },
                    Evidence {
                        label: "abs_threshold_bytes_per_min".into(),
                        value: cfg.disk_abs_bytes_per_min,
                        unit: "B/min".into(),
                    },
                    Evidence {
                        label: "peak_at_ms".into(),
                        value: peak_at as f64,
                        unit: "ms".into(),
                    },
                ],
                severity: "warn".into(),
                confidence: 0.85,
                baseline: None,
                related_event_ids: ctx.related_event_ids.clone(),
                note: "磁盘吞吐连续 ≥2 分钟超绝对阈值（单点抖动不报）".into(),
            }];
        }
    }
    Vec::new()
}

/// 短命进程反复：同一（pid 无关，按 name 聚合）进程近窗内 ≥ N 世、每世
/// 寿命 ≤ max_lifetime_s。`lifetimes` = (pid, name, exe_start_s, exit_s)。
pub fn detect_short_lived_process(
    lifetimes: &[(u32, String, u64, u64)],
    cfg: &DetectorConfig,
    ctx: &AnomalyCtx,
) -> Vec<ResourceAnomaly> {
    // 按 name 聚合：统计短命世数
    use std::collections::BTreeMap;
    let mut by_name: BTreeMap<&str, Vec<(u32, u64, u64)>> = BTreeMap::new();
    for (pid, name, start_s, exit_s) in lifetimes {
        by_name
            .entry(name.as_str())
            .or_default()
            .push((*pid, *start_s, *exit_s));
    }
    let mut out = Vec::new();
    for (name, incs) in by_name {
        let short: Vec<&(u32, u64, u64)> = incs
            .iter()
            .filter(|(_, st, ex)| ex.saturating_sub(*st) <= cfg.short_lived_max_lifetime_s)
            .collect();
        if (short.len() as u32) < cfg.short_lived_min_count {
            continue;
        }
        out.push(ResourceAnomaly {
            schema_version: RESOURCE_ANOMALY_SCHEMA_VERSION,
            kind: "short_lived_process".into(),
            metric_id: ctx.metric_id.to_string(),
            started_at_ms: (short.first().unwrap().1 as i64) * 1000,
            ended_at_ms: Some((short.last().unwrap().2 as i64) * 1000),
            process_identity: Some(ProcessIdentityLite {
                pid: short[0].0,
                name: name.to_string(),
                exe_start_epoch: short[0].1,
            }),
            evidence: vec![
                Evidence {
                    label: "short_lived_incarnations".into(),
                    value: short.len() as f64,
                    unit: "世".into(),
                },
                Evidence {
                    label: "max_lifetime_s".into(),
                    value: cfg.short_lived_max_lifetime_s as f64,
                    unit: "s".into(),
                },
            ],
            severity: "warn".into(),
            confidence: 0.85,
            baseline: None,
            related_event_ids: ctx.related_event_ids.clone(),
            note: "同名进程窗口内反复短命（崩溃循环/看门狗拉起特征）".into(),
        });
    }
    out
}

/// 单父多子：同一 parent 在近窗内派生 ≥ N 个不同 child pid。
/// `children` = (parent_pid, child_pid, t_ms)。
pub fn detect_process_fanout(
    children: &[(u32, u32, i64)],
    cfg: &DetectorConfig,
    ctx: &AnomalyCtx,
) -> Vec<ResourceAnomaly> {
    use std::collections::BTreeMap;
    let mut by_parent: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
    for (parent, child, _t) in children {
        let v = by_parent.entry(*parent).or_default();
        if !v.contains(child) {
            v.push(*child); // 同 child 重复行去重
        }
    }
    let mut out = Vec::new();
    for (parent, kids) in by_parent {
        if (kids.len() as u32) < cfg.fanout_min_children {
            continue;
        }
        out.push(ResourceAnomaly {
            schema_version: RESOURCE_ANOMALY_SCHEMA_VERSION,
            kind: "process_fanout".into(),
            metric_id: ctx.metric_id.to_string(),
            started_at_ms: 0,
            ended_at_ms: None,
            process_identity: Some(ProcessIdentityLite {
                pid: parent,
                name: format!("pid:{parent}"),
                exe_start_epoch: 0,
            }),
            evidence: vec![
                Evidence {
                    label: "distinct_children".into(),
                    value: kids.len() as f64,
                    unit: "个".into(),
                },
                Evidence {
                    label: "min_children".into(),
                    value: cfg.fanout_min_children as f64,
                    unit: "个".into(),
                },
            ],
            severity: "warn".into(),
            confidence: 0.8,
            baseline: None,
            related_event_ids: ctx.related_event_ids.clone(),
            note: "单父进程短窗内派生大量不同子进程（批量执行/恶意脚本特征）".into(),
        });
    }
    out
}

/// S0.2 告警通道事件折算（采集器失效/网络切换）：`events` =
/// (rowid, kind, t_ms)；kind 以 "collection" 开头 → collector_failure，
/// kind 含 "network" → network_switch，其余忽略。直通映射，channel 已是
/// 去重告警面（S0.2），此处不改写语义。
pub fn detect_channel_events(
    events: &[(i64, String, i64)],
    ctx: &AnomalyCtx,
) -> Vec<ResourceAnomaly> {
    events
        .iter()
        .filter_map(|(rowid, kind, t_ms)| {
            let kind_out = if kind.starts_with("collection") {
                "collector_failure"
            } else if kind.contains("network") {
                "network_switch"
            } else {
                return None; // 无关 kind 忽略
            };
            Some(ResourceAnomaly {
                schema_version: RESOURCE_ANOMALY_SCHEMA_VERSION,
                kind: kind_out.into(),
                metric_id: ctx.metric_id.to_string(),
                started_at_ms: *t_ms,
                ended_at_ms: None,
                process_identity: None,
                evidence: Vec::new(),
                severity: "warn".into(),
                confidence: 1.0,
                baseline: None,
                related_event_ids: vec![*rowid],
                note: format!("S0.2 告警通道事件直通折算（kind={kind}），语义不改写"),
            })
        })
        .collect()
}

/// 折算进现有 anomalies 展示/消费面（dash /api/anomalies、MCP get_anomalies
/// 同管道）：kind→kind、severity 直通、证据与基线拼 message/detail。
pub fn to_anomaly(r: &ResourceAnomaly) -> crate::anomaly::Anomaly {
    let ev = r
        .evidence
        .iter()
        .map(|e| format!("{}={:.4}{}", e.label, e.value, e.unit))
        .collect::<Vec<_>>()
        .join(", ");
    let bl = r
        .baseline
        .map(|b| format!("p50={:.4} p95={:.4}({}样本)", b.p50, b.p95, b.samples))
        .unwrap_or_else(|| "无基线".into());
    let ids = r
        .related_event_ids
        .iter()
        .map(|i| i.to_string())
        .collect::<Vec<_>>()
        .join(",");
    crate::anomaly::Anomaly {
        kind: r.kind.clone(),
        kind_label: "资源异常".into(),
        severity: r.severity.clone(),
        message: format!("{}: {}（confidence={:.2}）", r.kind, ev, r.confidence),
        detail: format!(
            "metric_id={} started={} ended={:?} baseline=[{}] related=[{}] note={} schema_version={}",
            r.metric_id,
            r.started_at_ms,
            r.ended_at_ms,
            bl,
            ids,
            r.note,
            r.schema_version
        ),
        at: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MB: f64 = 1024.0 * 1024.0;

    fn pt(min: i64, v: f64) -> SeriesPoint {
        SeriesPoint {
            t_ms: 1_800_000_000_000 + min * 60_000,
            v,
        }
    }

    fn cfg() -> DetectorConfig {
        DetectorConfig {
            traffic_abs_bytes_per_min: 100.0 * MB,
            disk_abs_bytes_per_min: 200.0 * MB,
            ..DetectorConfig::default()
        }
    }

    fn ctx(metric: &str) -> AnomalyCtx<'_> {
        AnomalyCtx {
            metric_id: metric,
            identity: Some(ProcessIdentityLite {
                pid: 42,
                name: "app.exe".into(),
                exe_start_epoch: 1_700_000_000,
            }),
            related_event_ids: vec![11, 22],
        }
    }

    fn hist(n: u32, v: f64) -> Vec<(u32, f64)> {
        (0..n).map(|_| (10u32, v)).collect()
    }

    // ── 基线 ──

    #[test]
    fn m41_percentile_linear_interpolation() {
        let s = [1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0, 10.0];
        assert_eq!(percentile(&s, 0.5), 5.5);
        assert_eq!(percentile(&s, 0.0), 1.0);
        assert_eq!(percentile(&s, 1.0), 10.0);
        assert!((percentile(&s, 0.95) - 9.55).abs() < 1e-9);
        assert_eq!(percentile(&[], 0.5), 0.0, "空序列 0 不 panic");
    }

    #[test]
    fn m41_hour_baseline_same_hour_only_and_min_samples() {
        let mut history: Vec<(u32, f64)> = hist(10, 100.0);
        history.push((11, 999999.0)); // 不同小时：不得混入
        let b = hour_baseline(&history, 10, 10).expect("样本足须有基线");
        assert_eq!(b.p50, 100.0);
        assert_eq!(b.p95, 100.0);
        assert_eq!(b.samples, 10);
        assert!(
            hour_baseline(&history[..5], 10, 10).is_none(),
            "样本不足不得设基线"
        );
        // 分位真实反映离群：9 个 100 + 1 个 500 → p95 > 100
        let mut h2 = hist(9, 100.0);
        h2.push((10, 500.0));
        let b2 = hour_baseline(&h2, 10, 10).unwrap();
        assert!(b2.p95 > 100.0 && b2.p50 <= 100.0 + f64::EPSILON);
    }

    // ── 组合门（防误报核心） ──

    #[test]
    fn m41_baseline_abs_gate_combo() {
        let c = cfg();
        let b = Baseline {
            p50: 10.0 * MB,
            p95: 30.0 * MB,
            samples: 20,
        };
        // 有基线：超绝对(100MB)但未超 p95×4=120MB → 不命中（防误报断言）
        assert!(baseline_abs_gate(110.0 * MB, Some(b), 100.0 * MB, &c).is_none());
        // 超绝对也超 p95×4 → 命中，confidence 满、基线随附
        let (conf, bl) = baseline_abs_gate(130.0 * MB, Some(b), 100.0 * MB, &c).expect("须命中");
        assert!(conf >= 0.9);
        assert_eq!(bl.unwrap().p95, 30.0 * MB);
        // 无基线：仅绝对门，confidence 下调
        let (conf2, bl2) = baseline_abs_gate(150.0 * MB, None, 100.0 * MB, &c).expect("须命中");
        assert!(conf2 < 0.8 && conf2 > 0.0);
        assert!(bl2.is_none());
        // 无基线且低于绝对门 → 不命中
        assert!(baseline_abs_gate(50.0 * MB, None, 100.0 * MB, &c).is_none());
    }

    // ── 流量 ──

    #[test]
    fn m41_traffic_surge_hit_and_miss() {
        let c = cfg();
        let b = Baseline {
            p50: 5.0 * MB,
            p95: 10.0 * MB,
            samples: 20,
        };
        // 命中：150MB 超绝对(100MB)也超 p95×4(40MB)
        let hit = detect_traffic_surge(
            &[pt(0, 10.0 * MB), pt(1, 150.0 * MB)],
            Some(b),
            &c,
            &ctx("net.host.bytes_out"),
        );
        assert_eq!(hit.len(), 1);
        assert_eq!(hit[0].kind, "traffic_surge");
        assert_eq!(hit[0].metric_id, "net.host.bytes_out");
        assert_eq!(hit[0].started_at_ms, 1_800_000_000_000 + 60_000);
        assert!(hit[0].process_identity.is_some());
        assert!(!hit[0].evidence.is_empty());
        assert_eq!(hit[0].schema_version, RESOURCE_ANOMALY_SCHEMA_VERSION);
        assert!(!hit[0].related_event_ids.is_empty());
        // 防误报：50MB 超 p95×4 但未超绝对 → 不报；10MB 全低于 → 不报
        assert!(detect_traffic_surge(&[pt(0, 50.0 * MB)], Some(b), &c, &ctx("x")).is_empty());
        assert!(detect_traffic_surge(&[pt(0, 10.0 * MB)], Some(b), &c, &ctx("x")).is_empty());
    }

    #[test]
    fn m41_sustained_upload_hit_needs_consecutive_minutes() {
        let c = DetectorConfig {
            sustained_upload_minutes: 3,
            ..cfg()
        };
        // 命中：3 连续分钟过门
        let hit = detect_sustained_upload(
            &[pt(0, 150.0 * MB), pt(1, 160.0 * MB), pt(2, 170.0 * MB)],
            None,
            &c,
            &ctx("net.host.bytes_out"),
        );
        assert_eq!(hit.len(), 1);
        assert_eq!(hit[0].kind, "sustained_upload");
        assert_eq!(hit[0].started_at_ms, pt(0, 0.0).t_ms);
        // 不命中：只有 2 连续（第三分钟回落）
        assert!(detect_sustained_upload(
            &[pt(0, 150.0 * MB), pt(1, 160.0 * MB), pt(2, 1.0 * MB)],
            None,
            &c,
            &ctx("x")
        )
        .is_empty());
    }

    // ── CPU ──

    #[test]
    fn m41_cpu_sustained_hit_and_miss() {
        let c = DetectorConfig {
            cpu_sustained_minutes: 3,
            ..cfg()
        };
        let high: Vec<SeriesPoint> = (0..3).map(|m| pt(m, 90.0)).collect();
        let hit = detect_cpu_sustained(&high, &c, &ctx("cpu.host.cpu_percent"));
        assert_eq!(hit.len(), 1);
        assert_eq!(hit[0].kind, "cpu_sustained");
        let sev = hit[0].severity.as_str();
        assert!(["warn", "alert"].contains(&sev));
        // 防误报：均值高但时长不足 / 时长足但均值低
        assert!(detect_cpu_sustained(&high[..2], &c, &ctx("x")).is_empty());
        let low: Vec<SeriesPoint> = (0..3).map(|m| pt(m, 40.0)).collect();
        assert!(detect_cpu_sustained(&low, &c, &ctx("x")).is_empty());
    }

    #[test]
    fn m41_cpu_periodic_spike_hit_and_random_miss() {
        let c = DetectorConfig {
            cpu_periodic_min_spikes: 3,
            ..cfg()
        };
        // 命中：每 10 分钟一个 99% 尖峰，共 3 次（等周期）
        let mut series = Vec::new();
        for i in 0..3 {
            series.push(pt(i * 10, 99.0));
            series.push(pt(i * 10 + 1, 5.0));
        }
        let hit = detect_cpu_periodic_spike(&series, &c, &ctx("cpu.host.cpu_percent"));
        assert_eq!(hit.len(), 1);
        assert_eq!(hit[0].kind, "cpu_periodic_spike");
        // 防误报：随机尖峰（间隔 5/25 分钟，变异系数大）不报
        let random = vec![pt(0, 99.0), pt(5, 99.0), pt(30, 99.0)];
        assert!(detect_cpu_periodic_spike(&random, &c, &ctx("x")).is_empty());
        // 尖峰次数不足不报
        assert!(detect_cpu_periodic_spike(&series[..2], &c, &ctx("x")).is_empty());
    }

    // ── 内存 ──

    #[test]
    fn m41_mem_growth_slope_hit_and_miss() {
        let c = DetectorConfig {
            mem_growth_mb_per_hour: 200.0,
            ..cfg()
        };
        // 命中：30 分钟涨 200MB → 400MB/h 斜率
        let rising = vec![pt(0, 100.0), pt(15, 200.0), pt(30, 300.0)];
        let hit = detect_mem_growth(&rising, &c, &ctx("mem.process.working_set_bytes"));
        assert_eq!(hit.len(), 1);
        assert_eq!(hit[0].kind, "mem_growth");
        assert!(hit[0].confidence > 0.0);
        // 防误报：平稳序列 / 跨窗不足 30 分钟
        let flat: Vec<SeriesPoint> = (0..4).map(|m| pt(m * 10, 100.0)).collect();
        assert!(detect_mem_growth(&flat, &c, &ctx("x")).is_empty());
        let short_rise = vec![pt(0, 100.0), pt(10, 200.0)];
        assert!(detect_mem_growth(&short_rise, &c, &ctx("x")).is_empty());
    }

    // ── 磁盘 ──

    #[test]
    fn m41_disk_throughput_hit_needs_sustained_two_minutes() {
        let c = cfg();
        let hit = detect_disk_throughput(
            &[pt(0, 300.0 * MB), pt(1, 300.0 * MB)],
            &c,
            &ctx("disk.host.io_write_bytes"),
        );
        assert_eq!(hit.len(), 1);
        assert_eq!(hit[0].kind, "disk_throughput");
        // 防误报：单分钟尖点不报
        assert!(
            detect_disk_throughput(&[pt(0, 300.0 * MB), pt(1, 1.0 * MB)], &c, &ctx("x")).is_empty()
        );
        // 绝对门内不报
        assert!(
            detect_disk_throughput(&[pt(0, 100.0 * MB), pt(1, 100.0 * MB)], &c, &ctx("x"))
                .is_empty()
        );
    }

    // ── 进程 ──

    #[test]
    fn m41_short_lived_process_hit_and_miss() {
        let c = DetectorConfig {
            short_lived_min_count: 3,
            short_lived_max_lifetime_s: 5,
            ..cfg()
        };
        let lives: Vec<(u32, String, u64, u64)> = (0..3)
            .map(|i: i64| {
                (
                    100u32 + i as u32,
                    "worker.exe".to_string(),
                    1_700_000_000u64 + (i as u64) * 60,
                    1_700_000_000u64 + (i as u64) * 60 + 2,
                )
            })
            .collect();
        let hit = detect_short_lived_process(&lives, &c, &ctx("process.lifetime"));
        assert_eq!(hit.len(), 1);
        assert_eq!(hit[0].kind, "short_lived_process");
        assert!(
            hit[0].evidence.iter().any(|e| e.value == 3.0),
            "证据须含世数"
        );
        // 防误报：寿命长（正常进程）不报
        let long_lived: Vec<(u32, String, u64, u64)> = (0..3)
            .map(|i: i64| {
                (
                    100u32 + i as u32,
                    "worker.exe".to_string(),
                    1_700_000_000u64 + (i as u64) * 3600,
                    1_700_000_000u64 + (i as u64) * 3600 + 3600,
                )
            })
            .collect();
        assert!(detect_short_lived_process(&long_lived, &c, &ctx("x")).is_empty());
        // 世数不足不报
        assert!(detect_short_lived_process(&lives[..2], &c, &ctx("x")).is_empty());
    }

    #[test]
    fn m41_process_fanout_hit_and_miss() {
        let c = DetectorConfig {
            fanout_min_children: 3,
            ..cfg()
        };
        let kids: Vec<(u32, u32, i64)> = (0..3)
            .map(|i: i64| (1000u32, 2000u32 + i as u32, 1_800_000_000_000i64 + i))
            .collect();
        let hit = detect_process_fanout(&kids, &c, &ctx("process.children"));
        assert_eq!(hit.len(), 1);
        assert_eq!(hit[0].kind, "process_fanout");
        assert!(hit[0].evidence.iter().any(|e| e.value == 3.0));
        // 防误报：同 child 重复行不重复计数
        let dup = vec![(1000u32, 2000u32, 1i64), (1000, 2000, 2), (1000, 2001, 3)];
        assert!(
            detect_process_fanout(&dup, &c, &ctx("x")).is_empty(),
            "去重后仅 2 子"
        );
    }

    // ── S0.2 通道 ──

    #[test]
    fn m41_channel_events_wrap_and_related_ids() {
        let events = vec![
            (
                101i64,
                "collection_dropped".to_string(),
                1_800_000_000_000i64,
            ),
            (102, "network_changed".to_string(), 1_800_000_060_000),
            (103, "unrelated_kind".to_string(), 1_800_000_120_000),
        ];
        let hit = detect_channel_events(&events, &ctx("collector.health"));
        assert_eq!(hit.len(), 2, "无关 kind 忽略");
        assert_eq!(hit[0].kind, "collector_failure");
        assert_eq!(hit[1].kind, "network_switch");
        assert_eq!(hit[0].related_event_ids, vec![101]);
        assert_eq!(hit[1].related_event_ids, vec![102]);
    }

    // ── 统一模型完整性 + 复用面折算 ──

    #[test]
    fn m41_resource_anomaly_fields_complete_and_to_anomaly() {
        let c = cfg();
        let hit = detect_traffic_surge(&[pt(1, 150.0 * MB)], None, &c, &ctx("net.host.bytes_out"));
        let a = &hit[0];
        // 字段完整断言（M4.1 统一事件契约）
        assert_eq!(a.schema_version, 1);
        assert!(!a.metric_id.is_empty());
        assert!(a.ended_at_ms.is_none(), "进行中未收口为 None");
        assert!(!a.evidence.is_empty(), "证据数字非空");
        assert!((0.0..=1.0).contains(&a.confidence), "confidence ∈ [0,1]");
        assert!(["info", "warn", "alert"].contains(&a.severity.as_str()));
        assert!(!a.note.is_empty());
        // 折算现有面：kind/severity 直通，message/detail 带证据，schema_version 保留在 detail
        let conv = to_anomaly(a);
        assert_eq!(conv.kind, a.kind);
        assert_eq!(conv.severity, a.severity);
        assert!(
            conv.message.contains("value_bytes_per_min"),
            "message 须带证据标签与数字"
        );
        assert!(
            conv.detail.contains("schema_version=1"),
            "detail 须带 schema_version"
        );
    }
}
