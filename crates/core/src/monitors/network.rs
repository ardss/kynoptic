//! 网络 IO 监控
//!
//! 通过 `netstat -e` 读取全局网络字节数与包计数（内核级累计值，自系统启动起，
//! 含所有物理/虚拟接口）。
//!
//! 此前用 GetIfTable2 + 硬编码 MIB_IF_ROW2 偏移读取，累计值偏大数个量级
//! （偏移/接口过滤问题）。netstat -e 的输出直接来自 IPHLPAPI 的内部统计，
//! 与 Get-NetAdapterStatistics 一致，且一次给出 bytes + packets，覆盖前端
//! NetworkView 所需的 bytes_sent/bytes_recv/packets_sent/packets_recv。
//! netstat 失败时降级回 FFI 读取（仅 delta 可信）。

use crate::types::*;
use serde_json::json;
use std::cell::Cell;
use std::time::Duration;

pub struct NetworkMonitor {
    last_bytes_sent: Cell<u64>,
    last_bytes_recv: Cell<u64>,
}

impl Default for NetworkMonitor {
    fn default() -> Self {
        Self {
            last_bytes_sent: Cell::new(0),
            last_bytes_recv: Cell::new(0),
        }
    }
}

impl Monitor for NetworkMonitor {
    fn name(&self) -> &str {
        "network"
    }
    fn interval(&self) -> Duration {
        Duration::from_secs(30)
    }

    fn collect(&self, tx: &crossbeam_channel::Sender<Event>) {
        let (bytes_sent, bytes_recv, packets_sent, packets_recv) = collect_network_io();

        let prev_sent = self.last_bytes_sent.get();
        let prev_recv = self.last_bytes_recv.get();

        self.last_bytes_sent.set(bytes_sent);
        self.last_bytes_recv.set(bytes_recv);

        // 首次采集或无流量时跳过
        if prev_sent == 0 && prev_recv == 0 {
            return;
        }
        let delta_sent = bytes_sent.saturating_sub(prev_sent);
        let delta_recv = bytes_recv.saturating_sub(prev_recv);
        if delta_sent == 0 && delta_recv == 0 {
            return;
        }

        // 直接序列化 ConnSnapshot struct —— 字段名（delta_sent/bytes_sent/packets_* 等）
        // 单一来源，前端 NetworkView 按此键名读取。packets_* 为 None 时自动省略。
        let snap = ConnSnapshot {
            delta_sent,
            delta_recv,
            bytes_sent,
            bytes_recv,
            packets_sent,
            packets_recv,
        };
        let data = serde_json::to_value(&snap).unwrap_or_else(|_| json!({}));
        let event = Event::new(EventAction::ConnSnapshot, EventType::Network).data(data);
        let _ = tx.try_send(event);
    }
}

/// conn_snapshot 的 event_data 结构（强类型，字段名即契约）。
///
/// 前端 NetworkView 读取 delta_*/bytes_*/packets_*。packets_* 仅 netstat 路径有值，
/// FFI 降级时为 None → 序列化时省略，前端判 null。
#[derive(serde::Serialize)]
struct ConnSnapshot {
    delta_sent: u64,
    delta_recv: u64,
    bytes_sent: u64,
    bytes_recv: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    packets_sent: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    packets_recv: Option<u64>,
}

/// 读取全局网络累计统计。
///
/// 优先用 `netstat -e`（输出形如下，键名随系统语言变化）：
/// ```text
///                            Received            Sent
/// Bytes                     880780388       568436904
/// Unicast packets              936756          783836
/// ```
/// 返回 (bytes_sent, bytes_recv, packets_sent?, packets_recv?)。
/// packets 为 None 表示该次未取到包计数。
fn collect_network_io() -> (u64, u64, Option<u64>, Option<u64>) {
    // 主路径：类型化 GetIfTable2（区域设置无关、零子进程）
    if let Some(out) = iftable2_stats() {
        return out;
    }
    // 兜底：netstat -e 文本解析（非中文系统可用）
    if let Some(out) = netstat_eth_stats() {
        return out;
    }
    (0, 0, None, None)
}

/// 调用 `netstat -e` 并解析输出。
fn netstat_eth_stats() -> Option<(u64, u64, Option<u64>, Option<u64>)> {
    let output = std::process::Command::new("netstat")
        .arg("-e")
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    parse_netstat_e(&text)
}

/// 解析 `netstat -e` 的文本输出，返回 (bytes_sent, bytes_recv, packets_sent?, packets_recv?)。
///
/// 纯函数：输入命令输出文本，输出统计数据，便于单元测试（多语言/空输出/异常）。
/// 按行首关键词匹配「字节/Bytes」「单播/Unicast packets」，提取该行末尾的两个整数
/// （顺序为 Received, Sent）。中英文系统键名都兼容。bytes 两项必须都有才算成功。
fn parse_netstat_e(text: &str) -> Option<(u64, u64, Option<u64>, Option<u64>)> {
    let mut bytes_recv: Option<u64> = None;
    let mut bytes_sent: Option<u64> = None;
    let mut pkts_recv: Option<u64> = None;
    let mut pkts_sent: Option<u64> = None;

    for line in text.lines() {
        let trimmed = line.trim_start();
        // 行首词：字节(中)/Bytes(英) —— 字节计数行
        let is_bytes = trimmed.starts_with("Bytes")
            || trimmed.starts_with("字节")
            || trimmed.starts_with("位元組");
        // 行首词：单播数据包(中)/Unicast packets(英) —— 包计数行
        let is_pkts = trimmed.starts_with("Unicast packets")
            || trimmed.starts_with("单播数据包")
            || trimmed.starts_with("單播封包");
        if !is_bytes && !is_pkts {
            continue;
        }
        // 提取行尾两个整数（Received 在前，Sent 在后）
        let nums: Vec<u64> = line
            .split_whitespace()
            .filter_map(|tok| tok.parse::<u64>().ok())
            .collect();
        if nums.len() >= 2 {
            // 顺序：Received, Sent
            if is_bytes {
                bytes_recv = Some(nums[nums.len() - 2]);
                bytes_sent = Some(nums[nums.len() - 1]);
            } else {
                pkts_recv = Some(nums[nums.len() - 2]);
                pkts_sent = Some(nums[nums.len() - 1]);
            }
        }
    }

    // bytes 两项必须都有，否则视为解析失败走降级。
    match (bytes_recv, bytes_sent) {
        (Some(r), Some(s)) => Some((s, r, pkts_sent, pkts_recv)),
        _ => None,
    }
}

/// GetIfTable2（类型化 MIB_IF_TABLE2）读取全接口流量统计。
///
/// 主路径（2026-09-09 probe 修复）：旧实现是"硬编码 MIB_IF_ROW2 字段偏移"的
/// 裸指针读取，ROW_SIZE/偏移在较新系统上与真实布局不符，实测（Win11 26300）
/// 恒返回 0 → delta 恒 0 → network 监控器静默零事件；同时 netstat -e 主路径
/// 在中文（OEM 代码页 GBK）系统上因 from_utf8_lossy 把"字节"行打成乱码而解析
/// 失败，两条路径同时失效。改为 windows-sys 类型化结构体：跨区域设置、
/// 零子进程、无偏移假设。回环接口（Type=24）不计入。
#[cfg(target_arch = "x86_64")]
fn iftable2_stats() -> Option<(u64, u64, Option<u64>, Option<u64>)> {
    use windows_sys::Win32::NetworkManagement::IpHelper::IF_TYPE_SOFTWARE_LOOPBACK;
    use windows_sys::Win32::NetworkManagement::IpHelper::{
        FreeMibTable, GetIfTable2, MIB_IF_TABLE2,
    };

    unsafe {
        let mut table_ptr: *mut MIB_IF_TABLE2 = std::ptr::null_mut();
        if GetIfTable2(&mut table_ptr) != 0 || table_ptr.is_null() {
            return None;
        }

        let num = (*table_ptr).NumEntries as usize;
        let mut total_sent: u64 = 0;
        let mut total_recv: u64 = 0;
        let mut pkts_sent: u64 = 0;
        let mut pkts_recv: u64 = 0;
        for row in std::slice::from_raw_parts((*table_ptr).Table.as_ptr(), num) {
            if row.Type == IF_TYPE_SOFTWARE_LOOPBACK {
                continue;
            }
            total_recv = total_recv.saturating_add(row.InOctets);
            total_sent = total_sent.saturating_add(row.OutOctets);
            pkts_recv = pkts_recv.saturating_add(row.InUcastPkts);
            pkts_sent = pkts_sent.saturating_add(row.OutUcastPkts);
        }
        FreeMibTable(table_ptr.cast());
        // 全零（无活动接口）视为不可用，交由调用方走 netstat 兜底
        if total_sent == 0 && total_recv == 0 {
            return None;
        }
        Some((total_sent, total_recv, Some(pkts_sent), Some(pkts_recv)))
    }
}

#[cfg(not(target_arch = "x86_64"))]
fn iftable2_stats() -> Option<(u64, u64, Option<u64>, Option<u64>)> {
    None
}

mod tests {
    #[allow(unused_imports)]
    use super::*;

    #[test]
    fn parse_netstat_e_english() {
        let out = "\
Interface Statistics

                           Received            Sent

Bytes                     880780388       568436904
Unicast packets              936756          783836
Non-unicast packets           29460            4860
";
        let (sent, recv, ps, pr) = parse_netstat_e(out).expect("english parse");
        assert_eq!(sent, 568436904);
        assert_eq!(recv, 880780388);
        assert_eq!(ps, Some(783836));
        assert_eq!(pr, Some(936756));
    }

    #[test]
    fn parse_netstat_e_chinese() {
        // 简体中文 Windows 输出
        let out = "\
接口统计信息

                           接收                发送

字节                     880780388       568436904
单播数据包                   936756          783836
";
        let (sent, recv, ps, pr) = parse_netstat_e(out).expect("chinese parse");
        assert_eq!(sent, 568436904);
        assert_eq!(recv, 880780388);
        assert_eq!(ps, Some(783836));
        assert_eq!(pr, Some(936756));
    }

    #[test]
    fn parse_netstat_e_bytes_only_no_packets() {
        // 没有 packets 行时，bytes 仍应解析成功，packets 为 None
        let out = "Bytes                     100       200\n";
        let (sent, recv, ps, pr) = parse_netstat_e(out).expect("bytes-only parse");
        assert_eq!((sent, recv), (200, 100));
        assert_eq!((ps, pr), (None, None));
    }

    #[test]
    fn parse_netstat_e_empty_or_garbage_returns_none() {
        assert!(parse_netstat_e("").is_none());
        assert!(parse_netstat_e("no relevant data here\n").is_none());
        // 只有 packets 没有 bytes 也算失败（bytes 是必需的）
        assert!(parse_netstat_e("Unicast packets   1  2\n").is_none());
    }

    /// 契约测试：ConnSnapshot 序列化键名必须与前端 NetworkView 一致。
    #[test]
    fn conn_snapshot_contract_keys() {
        let snap = ConnSnapshot {
            delta_sent: 100,
            delta_recv: 200,
            bytes_sent: 1000,
            bytes_recv: 2000,
            packets_sent: Some(10),
            packets_recv: Some(20),
        };
        let v = serde_json::to_value(&snap).unwrap();
        let obj = v.as_object().unwrap();
        // 前端读 delta_sent/recv 算速率，bytes_sent/recv 算累计
        assert!(obj.contains_key("delta_sent"), "缺 delta_sent");
        assert!(obj.contains_key("delta_recv"), "缺 delta_recv");
        assert!(obj.contains_key("bytes_sent"), "缺 bytes_sent");
        assert!(obj.contains_key("bytes_recv"), "缺 bytes_recv");
        assert!(obj.contains_key("packets_sent"), "缺 packets_sent");
        assert!(obj.contains_key("packets_recv"), "缺 packets_recv");
    }

    #[test]
    fn conn_snapshot_packets_none_omitted() {
        // FFI 降级路径 packets 为 None → 序列化省略，前端判 null
        let snap = ConnSnapshot {
            delta_sent: 100,
            delta_recv: 200,
            bytes_sent: 1000,
            bytes_recv: 2000,
            packets_sent: None,
            packets_recv: None,
        };
        let v = serde_json::to_value(&snap).unwrap();
        let obj = v.as_object().unwrap();
        assert!(
            !obj.contains_key("packets_sent"),
            "packets_sent 为 None 应省略"
        );
        assert!(
            !obj.contains_key("packets_recv"),
            "packets_recv 为 None 应省略"
        );
    }
}
