//! M3.3 套餐对账探查（无可信来源即 unavailable）。
//!
//! 定位：**只读**探查自由猫本机可用计量面，两条路：
//! 1. 本地 HTTP 端口常见路径探测（目标端口**仅限** M3.1 监听端口证据清单
//!    内的端口——[`http_targets_from_links`] 从证据生成，绝不扫描其它端口）；
//! 2. 安装目录只读配置/核心计数文件（路径由调用方经进程枚举推导后传入，
//!    本模块只做 `read_to_string` 级只读访问）。
//!
//! 信任口径（铁律）：只有**结构化用量字段**（`used`/`used_bytes` 数字，
//! 可选 `total`/`total_bytes`）才算可信接入（source=Provider）；HTML 页面、
//! 代理核心版本应答、实时速率（up/down）、任何非约定字段一律**不可信**。
//! **禁止用进程 I/O、网卡累计、连接数伪造套餐用量**——本模块没有任何此类
//! 回退路径，探不到就是 Unavailable，探查过程全量落 [`PlanProbeResult::steps`]
//! （调用方写进 limitations）。

use super::proxy_link::{PortProto, ProxyLinkEvidence};
use crate::metrics::{MetricPoint, MetricQuality, MetricSource};
use std::path::{Path, PathBuf};

/// 套餐用量指标 id（Provider 接入时）。
pub const METRIC_PLAN_USAGE_BYTES: &str = "plan.provider.usage_bytes";

/// HTTP 探测常见路径（自由猫本机控制面/计量面常见入口；只 GET 不改状态）。
pub const PROBE_HTTP_PATHS: &[&str] = &[
    "/",
    "/version",
    "/configs",
    "/status",
    "/metrics",
    "/usage",
    "/quota",
    "/v1/usage",
];

/// 一次探查动作记录（命令级事实，写 limitations 用）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeStep {
    pub target: String,
    pub outcome: String,
}

/// 可信结构化套餐用量。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlanUsage {
    pub used_bytes: u64,
    pub total_bytes: Option<u64>,
}

/// 探查结论。Unavailable = 无可信结构化来源（usage=None，steps 留痕）。
#[derive(Debug, Clone, PartialEq)]
pub struct PlanProbeResult {
    pub quality: MetricQuality,
    pub usage: Option<PlanUsage>,
    pub steps: Vec<ProbeStep>,
}

/// 探查传输面（可注入；真实实现见 [`RealProbeTransport`]）。
pub trait ProbeTransport {
    /// HTTP GET，返回响应体（含状态行错误的描述性 Err）。
    fn http_get(&mut self, url: &str) -> Result<String, String>;
    /// 只读读文件。
    fn read_file(&mut self, path: &Path) -> Result<String, String>;
}

/// 从 M3.1 代理端口证据生成 HTTP 探测目标（仅证据内端口；绝不额外扫描）。
pub fn http_targets_from_links(links: &[ProxyLinkEvidence]) -> Vec<String> {
    let mut out = Vec::new();
    for l in links {
        if l.proto != PortProto::Tcp {
            continue; // UDP 不是 HTTP 面
        }
        for path in PROBE_HTTP_PATHS {
            out.push(format!("http://127.0.0.1:{}{}", l.port, path));
        }
    }
    out
}

/// 可信判定：JSON 对象且含数字 `used_bytes` 或 `used`（可选 `total_bytes`/
/// `total`）。其余（HTML/版本应答/速率 up/down/未知字段结构）一律不可信。
pub fn parse_trusted_usage(body: &str) -> Option<PlanUsage> {
    let v: serde_json::Value = serde_json::from_str(body.trim()).ok()?;
    let obj = v.as_object()?;
    let num = |k: &str| obj.get(k).and_then(|x| x.as_u64());
    let used = num("used_bytes").or_else(|| num("used"))?;
    let total = num("total_bytes").or_else(|| num("total"));
    Some(PlanUsage {
        used_bytes: used,
        total_bytes: total,
    })
}

/// 探查：HTTP 目标逐个 GET、文件目标逐个只读读，先得可信结构化用量即收；
/// 全部落空 → Unavailable（steps 全量留痕，usage=None）。
pub fn probe_plan(
    t: &mut dyn ProbeTransport,
    http_targets: &[String],
    file_targets: &[PathBuf],
) -> PlanProbeResult {
    let mut steps = Vec::new();
    for url in http_targets {
        match t.http_get(url) {
            Ok(body) => match parse_trusted_usage(&body) {
                Some(u) => {
                    steps.push(ProbeStep {
                        target: url.clone(),
                        outcome: "可信结构化用量命中".into(),
                    });
                    return PlanProbeResult {
                        quality: MetricQuality::Confirmed,
                        usage: Some(u),
                        steps,
                    };
                }
                None => steps.push(ProbeStep {
                    target: url.clone(),
                    outcome: format!(
                        "应答不可信（非约定用量字段，已排除）：{}",
                        body.chars().take(80).collect::<String>()
                    ),
                }),
            },
            Err(e) => steps.push(ProbeStep {
                target: url.clone(),
                outcome: e,
            }),
        }
    }
    for f in file_targets {
        match t.read_file(f) {
            Ok(body) => match parse_trusted_usage(&body) {
                Some(u) => {
                    steps.push(ProbeStep {
                        target: f.display().to_string(),
                        outcome: "可信结构化计数命中（只读配置/计数文件）".into(),
                    });
                    return PlanProbeResult {
                        quality: MetricQuality::Confirmed,
                        usage: Some(u),
                        steps,
                    };
                }
                None => steps.push(ProbeStep {
                    target: f.display().to_string(),
                    outcome: "内容不可信（非约定用量字段，已排除）".into(),
                }),
            },
            Err(e) => steps.push(ProbeStep {
                target: f.display().to_string(),
                outcome: e,
            }),
        }
    }
    PlanProbeResult {
        quality: MetricQuality::Unavailable,
        usage: None,
        steps,
    }
}

/// Provider 指标点（仅可信接入时 Some；Unavailable 不产点——「未采到不是
/// 零」，Unavailable 状态由 [`PlanProbeResult`] 承载并写 limitations）。
pub fn plan_metric_point(
    result: &PlanProbeResult,
    host_id: &str,
    now_ms: i64,
) -> Option<MetricPoint> {
    let u = result.usage?;
    Some(MetricPoint {
        metric_id: METRIC_PLAN_USAGE_BYTES.to_string(),
        source: MetricSource::Provider,
        host_id: host_id.to_string(),
        process_identity: crate::metrics::ProcessIdentity::None,
        timestamp_utc_ms: now_ms,
        monotonic_ms: 0, // 探查时点语义无单调意义，恒 0（同 M2/M3.2 口径）
        interval_ms: 0,  // 探查为瞬时读数，无统计区间
        value: u.used_bytes as f64,
        unit: crate::metrics::MetricUnit::Bytes,
        scope: crate::metrics::MetricScope::Host,
        quality: MetricQuality::Confirmed,
        loss_count: None,
        coverage: None,
        schema_version: crate::metrics::METRIC_SCHEMA_VERSION,
    })
}

// ─── 真实传输面（只读；跨平台 std 实现）───

/// 真实探查传输：裸 TCP HTTP/1.1 GET（本地控制面通常 http；https 握手失败
/// 记为该目标不可达）+ `std::fs::read_to_string` 只读文件。
#[derive(Default)]
pub struct RealProbeTransport {
    buf: Vec<u8>,
}

impl RealProbeTransport {
    pub fn new() -> Self {
        Self::default()
    }
}

impl ProbeTransport for RealProbeTransport {
    fn http_get(&mut self, url: &str) -> Result<String, String> {
        // 解析 http://host:port/path（仅本地明文面；https 目标连接后 TLS
        // 握手必败，按不可达记录——不引 TLS 依赖）
        let _ = &self.buf;
        let rest = url
            .strip_prefix("http://")
            .ok_or_else(|| format!("非 http 目标（不支持 https 明文探测）: {url}"))?;
        let (hostport, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, "/"),
        };
        use std::io::{Read, Write};
        let mut stream = std::net::TcpStream::connect(hostport)
            .map_err(|e| format!("连接失败 {hostport}: {e}"))?;
        let req = format!(
            "GET {path} HTTP/1.1\r\nHost: {hostport}\r\nConnection: close\r\nAccept: */*\r\n\r\n"
        );
        stream
            .write_all(req.as_bytes())
            .map_err(|e| format!("请求写入失败 {url}: {e}"))?;
        let mut raw = Vec::new();
        stream
            .read_to_end(&mut raw)
            .map_err(|e| format!("应答读取失败 {url}: {e}"))?;
        let text = String::from_utf8_lossy(&raw);
        // 取状态行 + 剥离头部（探测只关心体；非 200 记状态）
        let sep = "\r\n\r\n";
        let (status, body) = match text.find(sep) {
            Some(i) => (
                text[..text.find('\n').unwrap_or(i)].trim(),
                text[i + sep.len()..].trim(),
            ),
            None => (text.trim(), ""),
        };
        if !status.contains("200") {
            return Err(format!("HTTP 非应答 200（{status}）"));
        }
        Ok(body.to_string())
    }

    fn read_file(&mut self, path: &Path) -> Result<String, String> {
        std::fs::read_to_string(path).map_err(|e| format!("只读失败 {path:?}: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::super::proxy_link::{PortProto, ProxyLinkEvidence};
    use super::*;
    use std::cell::RefCell;

    fn link(port: u16, proto: PortProto, pid: u32) -> ProxyLinkEvidence {
        ProxyLinkEvidence {
            proto,
            port,
            pid,
            process_name: Some("clash.exe".into()),
            exe_start_epoch: Some(1_700_000_000),
            quality: MetricQuality::Confirmed,
        }
    }

    /// 可脚本化假传输：按 url/路径回应。
    struct FakeTransport {
        http: RefCell<BTreeMap2<String, Result<String, String>>>,
        files: RefCell<BTreeMap2<String, Result<String, String>>>,
    }
    // 用 std BTreeMap（导入别名避免与外部冲突）
    type BTreeMap2<K, V> = std::collections::BTreeMap<K, V>;

    impl FakeTransport {
        fn empty() -> Self {
            Self {
                http: RefCell::new(BTreeMap2::new()),
                files: RefCell::new(BTreeMap2::new()),
            }
        }
    }

    impl ProbeTransport for FakeTransport {
        fn http_get(&mut self, url: &str) -> Result<String, String> {
            self.http
                .borrow_mut()
                .remove(url)
                .unwrap_or_else(|| Err(format!("conn refused {url}")))
        }
        fn read_file(&mut self, path: &Path) -> Result<String, String> {
            self.files
                .borrow_mut()
                .remove(&path.to_string_lossy().to_string())
                .unwrap_or_else(|| Err(format!("not found {path:?}")))
        }
    }

    #[test]
    fn m33_targets_built_only_from_evidence_ports() {
        let links = vec![
            link(7890, PortProto::Tcp, 100),
            link(9090, PortProto::Tcp, 100),
            link(51820, PortProto::Udp, 100), // UDP 不是 HTTP 面，不得生成目标
        ];
        let t = http_targets_from_links(&links);
        assert!(
            t.contains(&"http://127.0.0.1:7890/version".to_string()),
            "{t:?}"
        );
        assert!(t.contains(&"http://127.0.0.1:9090/".to_string()), "{t:?}");
        assert!(
            !t.iter().any(|u| u.contains("51820")),
            "UDP 端口不得生成 HTTP 探测目标"
        );
        assert!(
            t.iter().all(|u| u.starts_with("http://127.0.0.1:")),
            "目标必须限于本机证据端口"
        );
        assert!(http_targets_from_links(&[]).is_empty());
    }

    #[test]
    fn m33_trusted_parse_requires_structured_usage_fields() {
        assert_eq!(
            parse_trusted_usage(r#"{"used_bytes": 1073741824, "total_bytes": 10737418240}"#),
            Some(PlanUsage {
                used_bytes: 1_073_741_824,
                total_bytes: Some(10_737_418_240),
            })
        );
        assert_eq!(
            parse_trusted_usage(r#"{"used": 5, "total": 100}"#),
            Some(PlanUsage {
                used_bytes: 5,
                total_bytes: Some(100),
            })
        );
        // 不可信：HTML / 版本应答 / 实时速率 / 非数字 / 空体
        assert_eq!(parse_trusted_usage("<html>yacd</html>"), None);
        assert_eq!(parse_trusted_usage(r#"{"version":"1.18.0"}"#), None);
        assert_eq!(
            parse_trusted_usage(r#"{"up":123,"down":456}"#),
            None,
            "实时速率不是套餐用量"
        );
        assert_eq!(parse_trusted_usage(r#"{"used":"many"}"#), None);
        assert_eq!(parse_trusted_usage(""), None);
    }

    #[test]
    fn m33_trusted_usage_becomes_provider_metric_point() {
        let mut t = FakeTransport::empty();
        t.http.borrow_mut().insert(
            "http://127.0.0.1:9090/usage".into(),
            Ok(r#"{"used_bytes": 1073741824, "total_bytes": 10737418240}"#.into()),
        );
        let targets = vec!["http://127.0.0.1:9090/usage".to_string()];
        let r = probe_plan(&mut t, &targets, &[]);
        assert_eq!(r.quality, MetricQuality::Confirmed);
        assert_eq!(
            r.usage,
            Some(PlanUsage {
                used_bytes: 1_073_741_824,
                total_bytes: Some(10_737_418_240),
            })
        );
        let p =
            plan_metric_point(&r, "host-x", 1_800_000_000_000).expect("可信接入必须产 Provider 点");
        assert_eq!(p.metric_id, METRIC_PLAN_USAGE_BYTES);
        assert_eq!(p.source, MetricSource::Provider);
        assert_eq!(p.quality, MetricQuality::Confirmed);
        assert_eq!(p.value, 1_073_741_824.0);
    }

    #[test]
    fn m33_unavailable_when_no_trusted_source_and_no_fabrication() {
        // 全部落空：HTTP 全拒 + 文件全缺 → Unavailable、usage=None、
        // steps 全量留痕（limitations 素材）、不产点（未采到不是零）
        let mut t = FakeTransport::empty();
        t.http.borrow_mut().insert(
            "http://127.0.0.1:9090/version".into(),
            Ok(r#"{"version":"1.18.0","premium":true}"#.into()), // 版本应答≠用量
        );
        t.http.borrow_mut().insert(
            "http://127.0.0.1:9090/traffic".into(),
            Ok(r#"{"up":123,"down":456}"#.into()), // 速率≠套餐用量
        );
        let targets = vec![
            "http://127.0.0.1:9090/version".to_string(),
            "http://127.0.0.1:9090/traffic".to_string(),
            "http://127.0.0.1:9090/usage".to_string(), // conn refused
        ];
        let files = vec![PathBuf::from("C:/nonexistent/clash/config.yaml")];
        let r = probe_plan(&mut t, &targets, &files);
        assert_eq!(r.quality, MetricQuality::Unavailable);
        assert_eq!(r.usage, None, "无可信来源绝不伪造用量");
        assert!(
            r.steps.len() >= 4,
            "每个探查动作都要留痕（含失败），实际 {:?}",
            r.steps
        );
        assert!(
            r.steps.iter().any(|s| s.outcome.contains("conn refused")),
            "连接失败也要入账"
        );
        assert!(
            r.steps
                .iter()
                .any(|s| s.outcome.contains("速率") || s.outcome.contains("不可信")),
            "不可信负载的排除原因要入账"
        );
        assert!(
            plan_metric_point(&r, "h", 0).is_none(),
            "Unavailable 不产点（未采到不是零）"
        );
    }

    #[test]
    fn m33_config_file_trusted_schema_counts_too() {
        // 安装目录只读配置里的结构化计数（约定字段）同样可信
        let mut t = FakeTransport::empty();
        t.files.borrow_mut().insert(
            "C:/clash/traffic.json".into(),
            Ok(r#"{"used_bytes": 42}"#.into()),
        );
        let r = probe_plan(&mut t, &[], &[PathBuf::from("C:/clash/traffic.json")]);
        assert_eq!(r.quality, MetricQuality::Confirmed);
        assert_eq!(r.usage.map(|u| u.used_bytes), Some(42));
    }
}
