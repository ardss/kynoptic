//! M3.1 代理进程与监听端口识别（增强模式，不进 40 项基础集合）。
//!
//! 定位：**证据层**——识别本机代理客户端进程（进程名模式可配置）及其
//! 监听端口（GetExtendedTcpTable/GetExtendedUdpTable 按 PID 归属，身份
//! 归属复用 M2.3 [`ProcessIdentityCache`]）。识别结果只输出
//! process_identity + port 的客观证据，**不做任何假设性归类**
//! （不推断用途、不标注协议语义、不与采集事件强行关联）。
//!
//! 口径：
//! - 进程名匹配 = 大小写不敏感子串匹配（模式列表用户可增删，默认含常见
//!   自由猫/代理客户端进程名模式）；
//! - 监听端口 = TCP LISTEN（TCP_TABLE_OWNER_PID_LISTENER）+ UDP 全表
//!   （UDP_TABLE_OWNER_PID），v4/v6 双族；
//! - 端口表枚举不可用 → quality=Unavailable（显式降级，不静默为零）；
//! - 表中有 PID 但身份缓存无法归属（短命进程等）→ 该条 evidence
//!   quality=Incomplete，process_name 不伪造。

use super::net_etw::{ProcSnapshotSource, ProcessIdentityCache};
use crate::metrics::MetricQuality;

/// 默认代理客户端进程名模式（小写子串；用户可增删覆盖）。
pub const DEFAULT_PROXY_NAME_PATTERNS: &[&str] = &[
    "clash",
    "mihomo",
    "v2ray",
    "xray",
    "sing-box",
    "singbox",
    "shadowsocks",
    "ss-local",
    "sslocal",
    "shadowsocksr",
    "ssr-local",
    "trojan-go",
    "hysteria",
    "naive",
    "tuic",
    "juicity",
    "nekoray",
    "nekobox",
    "hiddify",
    "flclash",
    "v2rayn",
    "v2rayng",
    "leaf",
    "gost",
    "brook",
    "privoxy",
    "outline",
];

/// 代理识别配置（进程名模式列表，用户可增删；替换默认集）。
#[derive(Debug, Clone)]
pub struct ProxyLinkConfig {
    pub name_patterns: Vec<String>,
}

impl Default for ProxyLinkConfig {
    fn default() -> Self {
        Self {
            name_patterns: DEFAULT_PROXY_NAME_PATTERNS
                .iter()
                .map(|s| s.to_string())
                .collect(),
        }
    }
}

/// 大小写不敏感子串模式匹配（空模式列表 = 不匹配任何进程）。
pub fn name_matches(name: &str, patterns: &[String]) -> bool {
    let lower = name.to_lowercase();
    patterns
        .iter()
        .any(|p| !p.is_empty() && lower.contains(&p.to_lowercase()))
}

/// 端口协议。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortProto {
    Tcp,
    Udp,
}

/// 端口表行（端口 → PID 归属；v4/v6 同构）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListenRow {
    pub port: u16,
    pub pid: u32,
}

/// 解析 GetExtended*Table 返回的 OWNER_PID 表字节块：
/// `[dwNumEntries][row]...`，行内字段偏移由调用方按行布局给出
/// （端口字段为 DWORD 低 16 位、网络字节序）。仅产出 port>0 的行。
pub fn parse_owner_pid_table(
    buf: &[u8],
    row_len: usize,
    port_off: usize,
    pid_off: usize,
) -> Vec<ListenRow> {
    if buf.len() < 4 || row_len == 0 {
        return Vec::new();
    }
    let n = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    // 严格长度校验：截断表按实际可读行数解析（不越界读）
    let avail = (buf.len() - 4) / row_len;
    let n = n.min(avail);
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let row = &buf[4 + i * row_len..4 + (i + 1) * row_len];
        if row.len() < port_off + 4 || row.len() < pid_off + 4 {
            continue;
        }
        let port_dw = u32::from_le_bytes([
            row[port_off],
            row[port_off + 1],
            row[port_off + 2],
            row[port_off + 3],
        ]);
        // 端口存于 DWORD 低 16 位、网络字节序
        let port = ((port_dw & 0xFFFF) as u16).swap_bytes();
        let pid = u32::from_le_bytes([
            row[pid_off],
            row[pid_off + 1],
            row[pid_off + 2],
            row[pid_off + 3],
        ]);
        if port == 0 {
            continue;
        }
        out.push(ListenRow { port, pid });
    }
    out
}

/// TCP OWNER_PID 表行布局（MIB_TCPROW_OWNER_PID / MIB_TCP6ROW_OWNER_PID）：
/// v4 24B（state@0, localAddr@4, localPort@8, remoteAddr@12, remotePort@16,
/// pid@20）；v6 48B（state@0, localAddr@4..20, localPort@20, remoteAddr@24..40,
/// remotePort@40, pid@44）。TCP_LISTEN=2（含 state 字段的表按其过滤）。
pub fn parse_tcp_listen_rows(buf: &[u8], v6: bool) -> Vec<ListenRow> {
    const TCP_LISTEN: u32 = 2;
    let (row_len, port_off, pid_off) = if v6 { (48, 20, 44) } else { (24, 8, 20) };
    if buf.len() < 4 || row_len == 0 {
        return Vec::new();
    }
    let n = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    let avail = (buf.len() - 4) / row_len;
    let n = n.min(avail);
    let mut out = Vec::new();
    for i in 0..n {
        let row = &buf[4 + i * row_len..4 + (i + 1) * row_len];
        let state = u32::from_le_bytes([row[0], row[1], row[2], row[3]]);
        if state != TCP_LISTEN {
            continue;
        }
        let port_dw = u32::from_le_bytes([
            row[port_off],
            row[port_off + 1],
            row[port_off + 2],
            row[port_off + 3],
        ]);
        let port = ((port_dw & 0xFFFF) as u16).swap_bytes();
        if port == 0 {
            continue;
        }
        let pid = u32::from_le_bytes([
            row[pid_off],
            row[pid_off + 1],
            row[pid_off + 2],
            row[pid_off + 3],
        ]);
        out.push(ListenRow { port, pid });
    }
    out
}

/// UDP OWNER_PID 表行布局：v4 12B（addr@0, port@4, pid@8）；v6 24B
/// （addr@0..16, port@16, pid@20）。UDP 无连接，无 state 过滤。
pub fn parse_udp_rows(buf: &[u8], v6: bool) -> Vec<ListenRow> {
    let (row_len, port_off, pid_off) = if v6 { (24, 16, 20) } else { (12, 4, 8) };
    parse_owner_pid_table(buf, row_len, port_off, pid_off)
}

/// 端口表来源采集失败（权限/平台结局；无细分语义需求）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PortTableUnavailable;

/// 端口表来源（可注入；真实源见 [`RealPortTableSource`]）。
pub trait PortTableSource {
    /// 采集 TCP LISTEN + UDP 全部端口行（v4+v6）；不可用返回 Err。
    fn collect(&mut self) -> Result<Vec<(PortProto, ListenRow)>, PortTableUnavailable>;
}

/// 代理端口证据（客观事实：哪个进程在哪个端口监听）。
#[derive(Debug, Clone, PartialEq)]
pub struct ProxyLinkEvidence {
    pub proto: PortProto,
    pub port: u16,
    pub pid: u32,
    /// M2.3 身份缓存归属的进程名（归属失败的行不产出，不伪造名字）。
    pub process_name: Option<String>,
    pub exe_start_epoch: Option<u64>,
    /// 当前恒为 Confirmed（产出前提=名字模式匹配成立且身份归属成功）。
    pub quality: MetricQuality,
}

/// 一次识别快照。quality：端口表枚举不可用 = Unavailable（显式降级）；
/// 枚举成功 = 该次快照可用（无匹配属正常结局，仍 Confirmed 口径由各条
/// evidence 自带）。
#[derive(Debug, Clone, PartialEq)]
pub struct ProxyLinkSnapshot {
    pub quality: MetricQuality,
    pub links: Vec<ProxyLinkEvidence>,
}

/// 识别：刷新身份缓存 → 采集端口表 → 按 PID 归属身份 → 按进程名模式
/// 过滤出代理进程证据。不匹配的行与身份不可归属的行**均不输出**（无名字
/// 即无匹配依据，不做假设性归类）。
pub fn identify(
    cache: &mut ProcessIdentityCache,
    proc_src: &mut dyn ProcSnapshotSource,
    port_src: &mut dyn PortTableSource,
    cfg: &ProxyLinkConfig,
    now_epoch_s: u64,
) -> ProxyLinkSnapshot {
    cache.refresh(proc_src, now_epoch_s);
    let rows = match port_src.collect() {
        Ok(r) => r,
        Err(PortTableUnavailable) => {
            // 显式降级：枚举不可用不是「无代理」，不静默产出空证据
            return ProxyLinkSnapshot {
                quality: MetricQuality::Unavailable,
                links: Vec::new(),
            };
        }
    };
    let mut links = Vec::new();
    for (proto, row) in rows {
        // 身份归属（M2.3 缓存，按 now 时刻）；归属失败不硬归属
        let identity = cache.resolve(row.pid, now_epoch_s as i64);
        let name_matched = identity
            .map(|(name, _)| name_matches(name, &cfg.name_patterns))
            .unwrap_or(false);
        // 不匹配的行不输出：识别不做假设性归类（身份未知也不跳过模式判断
        // 之外的输出——名字未知的 PID 无法证匹配，同样不输出）
        if !name_matched {
            continue;
        }
        let (process_name, exe_start_epoch, quality) = match identity {
            Some((name, exe_start)) => (
                Some(name.to_string()),
                Some(exe_start),
                MetricQuality::Confirmed,
            ),
            None => (None, None, MetricQuality::Incomplete),
        };
        links.push(ProxyLinkEvidence {
            proto,
            port: row.port,
            pid: row.pid,
            process_name,
            exe_start_epoch,
            quality,
        });
    }
    ProxyLinkSnapshot {
        quality: MetricQuality::Confirmed,
        links,
    }
}

// ─── 真实端口表来源（windows-sys 直调，cfg(windows)）───

#[cfg(windows)]
pub struct RealPortTableSource {
    buf: Vec<u8>,
}

#[cfg(windows)]
impl RealPortTableSource {
    pub fn new() -> Self {
        Self { buf: Vec::new() }
    }
}

#[cfg(windows)]
impl Default for RealPortTableSource {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(windows)]
impl PortTableSource for RealPortTableSource {
    fn collect(&mut self) -> Result<Vec<(PortProto, ListenRow)>, PortTableUnavailable> {
        use windows_sys::Win32::NetworkManagement::IpHelper::{
            GetExtendedTcpTable, GetExtendedUdpTable, TCP_TABLE_OWNER_PID_LISTENER,
            UDP_TABLE_OWNER_PID,
        };
        // AF_INET=2 / AF_INET6=23（避免为此引入 WinSock feature）
        const AF_INET: u16 = 2;
        const AF_INET6: u16 = 23;
        const ERROR_INSUFFICIENT_BUFFER: u32 = 122;

        // 单族单类拉表：先探尺寸再取数据；行布局见解析函数文档。
        fn fetch(buf: &mut Vec<u8>, udp: bool, af: u32) -> Result<Vec<u8>, PortTableUnavailable> {
            let mut size: u32 = 0;
            let code = unsafe {
                if udp {
                    GetExtendedUdpTable(
                        std::ptr::null_mut(),
                        &mut size,
                        0,
                        af,
                        UDP_TABLE_OWNER_PID,
                        0,
                    )
                } else {
                    GetExtendedTcpTable(
                        std::ptr::null_mut(),
                        &mut size,
                        0,
                        af,
                        TCP_TABLE_OWNER_PID_LISTENER,
                        0,
                    )
                }
            };
            if code != ERROR_INSUFFICIENT_BUFFER && code != 0 {
                return Err(PortTableUnavailable);
            }
            if size == 0 {
                return Err(PortTableUnavailable);
            }
            buf.resize(size as usize, 0);
            let code = unsafe {
                if udp {
                    GetExtendedUdpTable(
                        buf.as_mut_ptr() as *mut core::ffi::c_void,
                        &mut size,
                        0,
                        af,
                        UDP_TABLE_OWNER_PID,
                        0,
                    )
                } else {
                    GetExtendedTcpTable(
                        buf.as_mut_ptr() as *mut core::ffi::c_void,
                        &mut size,
                        0,
                        af,
                        TCP_TABLE_OWNER_PID_LISTENER,
                        0,
                    )
                }
            };
            if code != 0 {
                return Err(PortTableUnavailable);
            }
            Ok(buf[..size as usize].to_vec())
        }

        let mut out = Vec::new();
        for (udp, af, v6) in [
            (false, u32::from(AF_INET), false),
            (false, u32::from(AF_INET6), true),
            (true, u32::from(AF_INET), false),
            (true, u32::from(AF_INET6), true),
        ] {
            let table = fetch(&mut self.buf, udp, af).map_err(|_| PortTableUnavailable)?;
            let rows = if udp {
                parse_udp_rows(&table, v6)
            } else {
                parse_tcp_listen_rows(&table, v6)
            };
            let proto = if udp { PortProto::Udp } else { PortProto::Tcp };
            out.extend(rows.into_iter().map(|r| (proto, r)));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::super::net_etw::ProcEntry;
    use super::*;
    use std::cell::RefCell;

    /// 合成 TCP OWNER_PID 表字节块（v4）：state, localAddr, localPort(网络
    /// 字节序低 16 位), remoteAddr, remotePort, pid。
    fn tcp_v4_row(state: u32, port: u16, pid: u32) -> Vec<u8> {
        let mut r = Vec::new();
        r.extend_from_slice(&state.to_le_bytes());
        r.extend_from_slice(&[127, 0, 0, 1]);
        r.extend_from_slice(&(port.swap_bytes() as u32).to_le_bytes());
        r.extend_from_slice(&[0, 0, 0, 0]);
        r.extend_from_slice(&0u32.to_le_bytes());
        r.extend_from_slice(&pid.to_le_bytes());
        assert_eq!(r.len(), 24);
        r
    }

    fn table(rows: Vec<Vec<u8>>) -> Vec<u8> {
        let mut b = (rows.len() as u32).to_le_bytes().to_vec();
        for r in rows {
            b.extend_from_slice(&r);
        }
        b
    }

    #[test]
    fn m31_parse_tcp_v4_table_maps_port_to_pid_listen_only() {
        // 7890（0x1ED2）LISTEN pid=4242 + 3389 非监听行被过滤 + port=0 丢弃
        let buf = table(vec![
            tcp_v4_row(2, 7890, 4242),
            tcp_v4_row(5, 3389, 999), // ESTABLISHED：非监听
            tcp_v4_row(2, 0, 7),      // 端口 0：丢弃
        ]);
        let rows = parse_tcp_listen_rows(&buf, false);
        assert_eq!(
            rows,
            vec![ListenRow {
                port: 7890,
                pid: 4242
            }],
            "只留 LISTEN 且端口非零"
        );
    }

    #[test]
    fn m31_parse_tcp_v6_layout() {
        // v6 行 52B：state@0, localAddr 16B@4, localPort@20, remote 20B@24, pid@48
        let mut b = vec![];
        b.extend_from_slice(&1u32.to_le_bytes()); // 1 行
        let mut r = vec![];
        r.extend_from_slice(&2u32.to_le_bytes());
        r.extend_from_slice(&[0u8; 16]);
        r.extend_from_slice(&(1080u16.swap_bytes() as u32).to_le_bytes());
        r.extend_from_slice(&[0u8; 20]);
        r.extend_from_slice(&777u32.to_le_bytes());
        assert_eq!(r.len(), 48);
        b.extend_from_slice(&r);
        assert_eq!(
            parse_tcp_listen_rows(&b, true),
            vec![ListenRow {
                port: 1080,
                pid: 777
            }]
        );
    }

    #[test]
    fn m31_parse_udp_v4_and_v6() {
        // v4 12B：addr@0, port@4, pid@8；v6 24B：addr 16B@0, port@16, pid@20
        let mut b = 2u32.to_le_bytes().to_vec();
        let mut r1 = vec![9u8; 4];
        r1.extend_from_slice(&(51820u16.swap_bytes() as u32).to_le_bytes());
        r1.extend_from_slice(&31337u32.to_le_bytes());
        assert_eq!(r1.len(), 12);
        b.extend_from_slice(&r1);
        assert_eq!(
            parse_udp_rows(&b, false),
            vec![ListenRow {
                port: 51820,
                pid: 31337
            }]
        );

        let mut b6 = 1u32.to_le_bytes().to_vec();
        let mut r2 = vec![9u8; 16];
        r2.extend_from_slice(&(53u16.swap_bytes() as u32).to_le_bytes());
        r2.extend_from_slice(&88u32.to_le_bytes());
        assert_eq!(r2.len(), 24);
        b6.extend_from_slice(&r2);
        assert_eq!(
            parse_udp_rows(&b6, true),
            vec![ListenRow { port: 53, pid: 88 }]
        );
    }

    #[test]
    fn m31_name_pattern_matching_case_insensitive_and_user_overridable() {
        let cfg = ProxyLinkConfig::default();
        assert!(name_matches("Clash Verge Service.exe", &cfg.name_patterns));
        assert!(name_matches("mihomo-windows-amd64.exe", &cfg.name_patterns));
        assert!(name_matches("SING-BOX.EXE", &cfg.name_patterns));
        assert!(!name_matches("chrome.exe", &cfg.name_patterns));
        assert!(!name_matches("explorer.exe", &cfg.name_patterns));

        // 用户增删：删默认、只留自定义模式
        let custom = ProxyLinkConfig {
            name_patterns: vec!["mytunnel".into()],
        };
        assert!(name_matches("MyTunnel.exe", &custom.name_patterns));
        assert!(!name_matches("clash.exe", &custom.name_patterns));

        // 用户在默认之上追加
        let mut extended = ProxyLinkConfig::default();
        extended.name_patterns.push("mytunnel".into());
        assert!(name_matches("MyTunnel.exe", &extended.name_patterns));
        assert!(name_matches("v2ray.exe", &extended.name_patterns));

        // 空模式 = 不匹配任何进程
        assert!(!name_matches("clash.exe", &[]));
    }

    struct FakeProc {
        entries: Vec<ProcEntry>,
    }
    impl ProcSnapshotSource for FakeProc {
        fn snapshot(&mut self) -> Vec<ProcEntry> {
            std::mem::take(&mut self.entries)
        }
    }

    struct FakePorts {
        rows: RefCell<Vec<(PortProto, ListenRow)>>,
        fail: bool,
    }
    impl PortTableSource for FakePorts {
        fn collect(&mut self) -> Result<Vec<(PortProto, ListenRow)>, PortTableUnavailable> {
            if self.fail {
                return Err(PortTableUnavailable);
            }
            Ok(std::mem::take(&mut *self.rows.borrow_mut()))
        }
    }

    fn proc_entry(pid: u32, name: &str) -> ProcEntry {
        ProcEntry {
            pid,
            name: name.into(),
            exe_start_epoch: 1_700_000_000,
        }
    }

    #[test]
    fn m31_identify_outputs_evidence_with_identity_no_speculative_rows() {
        // clash 在 7890/TCP + 7891/UDP 监听（身份归属成功，Confirmed）；
        // chrome 在 8443 监听但名字不匹配 → 不输出（不做假设性归类）
        let rows = vec![
            (
                PortProto::Tcp,
                ListenRow {
                    port: 7890,
                    pid: 100,
                },
            ),
            (
                PortProto::Udp,
                ListenRow {
                    port: 7891,
                    pid: 100,
                },
            ),
            (
                PortProto::Tcp,
                ListenRow {
                    port: 8443,
                    pid: 200,
                },
            ),
        ];
        let mut cache = ProcessIdentityCache::default();
        let mut proc_src = FakeProc {
            entries: vec![
                proc_entry(100, "Clash Verge.exe"),
                proc_entry(200, "chrome.exe"),
            ],
        };
        let mut port_src = FakePorts {
            rows: RefCell::new(rows),
            fail: false,
        };
        let snap = identify(
            &mut cache,
            &mut proc_src,
            &mut port_src,
            &ProxyLinkConfig::default(),
            1_700_000_100,
        );
        assert_ne!(snap.quality, MetricQuality::Unavailable);
        assert_eq!(
            snap.links,
            vec![
                ProxyLinkEvidence {
                    proto: PortProto::Tcp,
                    port: 7890,
                    pid: 100,
                    process_name: Some("Clash Verge.exe".into()),
                    exe_start_epoch: Some(1_700_000_000),
                    quality: MetricQuality::Confirmed,
                },
                ProxyLinkEvidence {
                    proto: PortProto::Udp,
                    port: 7891,
                    pid: 100,
                    process_name: Some("Clash Verge.exe".into()),
                    exe_start_epoch: Some(1_700_000_000),
                    quality: MetricQuality::Confirmed,
                },
            ],
            "只输出模式匹配的进程证据；非匹配进程的端口绝不输出"
        );
    }

    #[test]
    fn m31_port_enumeration_failure_is_unavailable() {
        let mut cache = ProcessIdentityCache::default();
        let mut proc_src = FakeProc { entries: vec![] };
        let mut port_src = FakePorts {
            rows: RefCell::new(vec![]),
            fail: true,
        };
        let snap = identify(
            &mut cache,
            &mut proc_src,
            &mut port_src,
            &ProxyLinkConfig::default(),
            1_700_000_100,
        );
        assert_eq!(
            snap.quality,
            MetricQuality::Unavailable,
            "枚举不可用必须显式降级"
        );
        assert!(snap.links.is_empty(), "不可用时不产出任何伪造证据");
    }

    #[test]
    fn m31_identity_unresolved_row_is_dropped_not_fabricated() {
        // pid=300 身份缓存无覆盖（表查询与快照之间退出的短命进程）：无名字
        // 即无匹配依据，行不输出（不出「未知名单」、不伪造证据）
        let rows = vec![(
            PortProto::Tcp,
            ListenRow {
                port: 7890,
                pid: 300,
            },
        )];
        let mut cache = ProcessIdentityCache::default();
        let mut proc_src = FakeProc { entries: vec![] };
        let mut port_src = FakePorts {
            rows: RefCell::new(rows),
            fail: false,
        };
        let snap = identify(
            &mut cache,
            &mut proc_src,
            &mut port_src,
            &ProxyLinkConfig::default(),
            1_700_000_100,
        );
        assert!(
            snap.links.is_empty(),
            "身份不可归属的行不得产出（无名字无匹配依据，不做假设性归类）"
        );
        assert_ne!(snap.quality, MetricQuality::Unavailable, "枚举本身成功");
    }

    #[test]
    fn m31_real_port_source_smoke_on_windows() {
        // 真实 API 冒烟：只要求不 panic（Err=权限/平台结局，合法）
        let mut src = RealPortTableSource::new();
        let _ = src.collect();
    }
}
