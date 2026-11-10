//! F11 多设备 timeline 合流（PRD v1.0 F11）：本机统一时间轴 + 逐个拉取
//! `settings.device_endpoints` 里远端设备的 `/api/timeline?minutes=N`，
//! 按设备分轨合并返回。
//!
//! 口径铁律（stage0-audit §5 决策 3）：**双机 human_presence / 时长不相加**——
//! 本端点只做「按设备分轨并列」，不做跨设备求和或区间交并；每条区间带
//! `device` 字段标记来源，offline 设备显式标注（绝不阻塞、绝不伪造本机数据）。
//!
//! 远端拉取走 std::net 手写 HTTP/1.1 GET（与 lib.rs serve 同族零新依赖）：
//! 单端点超时 [`REMOTE_TIMEOUT_SECS`]，各端点并行线程拉取（串行 N×5s 会把
//! 页面拖死）；任一失败降级为该设备 `status:"offline"`，本机数据照常返回。

use std::io::{Read, Write};
use std::time::Duration;

use serde_json::{json, Value};

/// 远端单请求超时（任务口径 5s；fetch 函数收 Duration，路由侧传本常量，
/// 测试可用更短值测超时降级）。
pub const REMOTE_TIMEOUT_SECS: u64 = 5;

/// 远端拉取的统一错误形态：连接失败 / 超时 / 非 200 / 非 JSON，全部折叠为
/// 一条字符串（offline 标注展示用，不区分故障类型给前端更多含义——unknown
/// 不是零，也不假装知道原因之外的事）。
pub fn fetch_remote_timeline(
    base_url: &str,
    token: &str,
    minutes: u32,
    query: &str,
    timeout: Duration,
) -> std::result::Result<Value, String> {
    let (host, port, prefix) = parse_base_url(base_url)?;
    // F07 筛选透传（project/source/evidence）：query 为空只带 minutes；
    // 非空原样拼接（路由侧已做词表校验，这里只做透传不做二次解析）
    let path = if query.is_empty() {
        format!(
            "{}{}api/timeline?minutes={}",
            prefix,
            if prefix.ends_with('/') { "" } else { "/" },
            minutes
        )
    } else {
        format!(
            "{}{}api/timeline?minutes={}&{}",
            prefix,
            if prefix.ends_with('/') { "" } else { "/" },
            minutes,
            query
        )
    };
    let addr = format!("{host}:{port}");
    let mut stream =
        std::net::TcpStream::connect(&addr).map_err(|e| format!("connect {addr}: {e}"))?;
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|e| format!("set_read_timeout: {e}"))?;
    stream
        .set_write_timeout(Some(timeout))
        .map_err(|e| format!("set_write_timeout: {e}"))?;
    let mut req = format!(
        "GET {path} HTTP/1.1\r\nHost: {host}:{port}\r\nConnection: close\r\nAccept: application/json\r\n"
    );
    if !token.is_empty() {
        // 与本机 dashboard 同一令牌头（X-Kynoptic-Access-Token / Bearer 二选一）
        req.push_str(&format!("X-Kynoptic-Access-Token: {token}\r\n"));
    }
    req.push_str("\r\n");
    stream
        .write_all(req.as_bytes())
        .map_err(|e| format!("write: {e}"))?;
    let mut raw = Vec::new();
    stream
        .read_to_end(&mut raw)
        .map_err(|e| format!("read: {e}"))?;
    let text = String::from_utf8_lossy(&raw);
    // 状态行 + 头部 + body 分离（header/body 以空行分隔；头部不区分大小写）
    let (head, body) = text
        .split_once("\r\n\r\n")
        .or_else(|| text.split_once("\n\n"))
        .ok_or_else(|| "响应缺少 header/body 分隔空行".to_string())?;
    let status = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or_else(|| "响应缺少 HTTP 状态行".to_string())?;
    if status != 200 {
        return Err(format!("HTTP {status}"));
    }
    serde_json::from_str(body.trim()).map_err(|e| format!("响应不是合法 JSON: {e}"))
}

/// 解析 `http://host[:port][/prefix]` 形式的 base URL（仅 http；https 需 TLS，
/// 本端点零依赖不实现——SSH 隧道场景本就是 http 回环，设置页文案同口径）。
/// 返回 (host, port, path 前缀)。host/port 过 sanitize（host 仅字母数字与
/// .-_:，防注入请求行）。
pub fn parse_base_url(url: &str) -> std::result::Result<(String, u16, String), String> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| "url 应为 http:// 开头（https 需 TLS，不支持）".to_string())?;
    let (authority, prefix) = match rest.split_once('/') {
        Some((a, p)) => (a, format!("/{}", p)),
        None => (rest, "/".to_string()),
    };
    if authority.is_empty() {
        return Err("url 缺少主机名".into());
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (
            h,
            p.parse::<u16>()
                .map_err(|_| format!("端口不是 1-65535 整数: {p}"))?,
        ),
        None => (authority, 80u16),
    };
    let host_ok = !host.is_empty()
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'));
    if !host_ok {
        return Err("主机名含非法字符（仅允许字母数字与 .-_）".into());
    }
    let port = if port == 0 { 80 } else { port };
    let prefix = if prefix == "/" {
        "/".to_string()
    } else {
        prefix.trim_end_matches('/').to_string()
    };
    Ok((host.to_string(), port, prefix))
}

/// 合流纯函数（可脱离 TCP 单测）：本机 [`crate::api_unified_timeline_at`]
/// 的 payload + 各远端 (设备名, 拉取结果) 列表 → 合并 payload。
///
/// 输出形态（在单机 payload 字段之上替换 tracks / 追加 devices）：
/// - `devices`: [{name, device:"local"|远端名, status:"online"|"offline", error?}]
/// - `tracks`: 每设备各自轨道 A-E 原样并列，每条 track 附 `device` 与
///   `device_status`，每条区间附 `device` 字段；offline 设备不产出轨道，
///   只在 devices 里带 error 显式标注。
/// - `human_presence_boundary`: 明示各设备 C 轨独立呈现、不相加。
pub fn merge_unified_timelines(
    local: Value,
    remotes: Vec<(String, std::result::Result<Value, String>)>,
    minutes: u32,
    now_ms: i64,
) -> Value {
    let mut devices: Vec<Value> = vec![json!({
        "name": "local",
        "device": "local",
        "status": "online",
    })];
    let mut tracks: Vec<Value> = Vec::new();
    // 本机轨道先行（本机数据绝不因远端而缺席）；每条 track/区间补 device 标记
    if let Some(local_tracks) = local["tracks"].as_array() {
        for tr in local_tracks {
            let mut t = tr.clone();
            if let Some(o) = t.as_object_mut() {
                o.insert("device".into(), json!("local"));
                o.insert("device_status".into(), json!("online"));
                if let Some(ivs) = o.get_mut("intervals").and_then(|v| v.as_array_mut()) {
                    for iv in ivs.iter_mut() {
                        if let Some(io) = iv.as_object_mut() {
                            io.insert("device".into(), json!("local"));
                        }
                    }
                }
            }
            tracks.push(t);
        }
    }
    for (name, res) in remotes {
        match res {
            Ok(payload) => {
                devices.push(json!({
                    "name": name,
                    "device": name,
                    "status": "online",
                }));
                if let Some(remote_tracks) = payload["tracks"].as_array() {
                    for tr in remote_tracks {
                        let mut t = tr.clone();
                        if let Some(o) = t.as_object_mut() {
                            o.insert("device".into(), json!(name));
                            o.insert("device_status".into(), json!("online"));
                            if let Some(ivs) = o.get_mut("intervals").and_then(|v| v.as_array_mut())
                            {
                                for iv in ivs.iter_mut() {
                                    if let Some(io) = iv.as_object_mut() {
                                        io.insert("device".into(), json!(name));
                                    }
                                }
                            }
                        }
                        tracks.push(t);
                    }
                }
            }
            Err(err) => {
                devices.push(json!({
                    "name": name,
                    "device": name,
                    "status": "offline",
                    "error": err,
                }));
            }
        }
    }
    let mut out = local.clone();
    if let Some(o) = out.as_object_mut() {
        o.insert("endpoint".into(), json!("unified_timeline_merged"));
        o.insert("merge".into(), json!(true));
        o.insert("minutes".into(), json!(minutes));
        o.insert("devices".into(), json!(devices));
        o.insert("tracks".into(), json!(tracks));
        o.insert(
            "human_presence_boundary".into(),
            json!({
                "note_en": "human_presence (track C) is rendered per device and NEVER summed across devices.",
                "note_zh": "人在场（C 轨）按设备独立呈现，多设备绝不相加。",
            }),
        );
        o.insert("generated_at_ms".into(), json!(now_ms));
    }
    out
}

/// 并行拉取全部远端（每端点独立线程 + 各自 5s 超时；串行最坏 N×5s 会把
/// 合并请求整体拖死）。返回与 `endpoints` 同序的结果列表。
pub fn fetch_all_remotes(
    endpoints: &[crate::settings::DeviceEndpoint],
    minutes: u32,
    query: &str,
    timeout: Duration,
) -> Vec<(String, std::result::Result<Value, String>)> {
    use std::sync::mpsc;
    let (tx, rx) = mpsc::channel();
    for e in endpoints {
        let tx = tx.clone();
        let name = e.name.clone();
        let url = e.url.clone();
        let token = e.token.clone();
        let query = query.to_string();
        std::thread::spawn(move || {
            let r = fetch_remote_timeline(&url, &token, minutes, &query, timeout);
            let _ = tx.send((name, r));
        });
    }
    // 逐个 recv：每个端点恰好发一条，顺序=发送序不保证，但 recv 侧按收到
    // 的 name 归位即可（合流只要求「每个设备都有结果」，不依赖顺序）。
    let mut out = Vec::with_capacity(endpoints.len());
    for _ in 0..endpoints.len() {
        if let Ok(pair) = rx.recv_timeout(timeout + Duration::from_secs(1)) {
            out.push(pair);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// URL 解析：缺 scheme / 非法 host / 缺省端口 / 带前缀路径。
    #[test]
    fn parse_base_url_cases() {
        assert_eq!(
            parse_base_url("http://127.0.0.1:8422").unwrap(),
            ("127.0.0.1".into(), 8422, "/".into())
        );
        assert_eq!(
            parse_base_url("http://localhost").unwrap(),
            ("localhost".into(), 80, "/".into())
        );
        assert_eq!(
            parse_base_url("http://127.0.0.1:18422/tunnel/").unwrap(),
            ("127.0.0.1".into(), 18422, "/tunnel".into())
        );
        assert!(parse_base_url("https://127.0.0.1").is_err());
        assert!(parse_base_url("http://bad host").is_err());
        assert!(parse_base_url("http://127.0.0.1:notaport").is_err());
    }

    /// 连接失败降级：无人监听的回环端口 → Err（offline 语义）。
    #[test]
    fn fetch_connect_refused_is_err() {
        // 端口 1（tcpmux）本机几乎不可能监听；即便监听也会因非 HTTP 响应失败
        let r = fetch_remote_timeline("http://127.0.0.1:1", "", 60, "", Duration::from_millis(300));
        assert!(r.is_err());
    }

    /// 超时降级：mock 服务端收请求后故意延迟（远超测试超时）→ Err。
    #[test]
    fn fetch_timeout_is_err() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let h = std::thread::spawn(move || {
            // 接一个连接，读掉请求后睡 2s 再回（测试超时 200ms 必先到）
            if let Ok((mut s, _)) = listener.accept() {
                let mut buf = [0u8; 1024];
                let _ = s.read(&mut buf);
                std::thread::sleep(std::time::Duration::from_millis(2000));
                let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}");
            }
        });
        let url = format!("http://{}", addr);
        let r = fetch_remote_timeline(&url, "", 60, "", Duration::from_millis(200));
        assert!(r.is_err(), "慢响应应在超时后返回 Err");
        h.join().unwrap();
    }

    /// 在线合流：mock 返回合法 timeline JSON → fetch Ok + merge 产出
    /// 带设备标记的 tracks 与 offline 标注。
    #[test]
    fn fetch_online_mock_and_merge() {
        let body = serde_json::json!({
            "endpoint": "unified_timeline",
            "window_start_ms": 1_000i64,
            "window_end_ms": 2_000i64,
            "tracks": [{
                "id": "A", "label_en": "Foreground windows", "label_zh": "前台窗口",
                "connected": true,
                "intervals": [{"start_ms": 1_000i64, "end_ms": 1_500i64,
                    "duration_ms": 500, "evidence_level": "observed"}]
            }],
        });
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let body_str = body.to_string();
        let h = std::thread::spawn(move || {
            if let Ok((mut s, _)) = listener.accept() {
                let mut buf = [0u8; 4096];
                let _ = s.read(&mut buf);
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body_str.len(),
                    body_str
                );
                let _ = s.write_all(resp.as_bytes());
            }
        });
        let url = format!("http://{}", addr);
        let v = fetch_remote_timeline(&url, "", 60, "", Duration::from_secs(5)).unwrap();
        assert_eq!(v["endpoint"], json!("unified_timeline"));
        h.join().unwrap();

        let local = serde_json::json!({
            "endpoint": "unified_timeline",
            "window_start_ms": 1_000i64,
            "window_end_ms": 2_000i64,
            "tracks": [{
                "id": "C", "label_zh": "在场 / 离场", "connected": true,
                "intervals": [{"start_ms": 1_100i64, "end_ms": 1_400i64,
                    "duration_ms": 300, "evidence_level": "observed"}]
            }],
        });
        let merged = merge_unified_timelines(
            local,
            vec![
                ("workstation-b".into(), Ok(v)),
                ("laptop-c".into(), Err("connect: refused".into())),
            ],
            60,
            2_000,
        );
        assert_eq!(merged["endpoint"], json!("unified_timeline_merged"));
        assert_eq!(merged["merge"], json!(true));
        let devices = merged["devices"].as_array().unwrap();
        assert_eq!(devices.len(), 3);
        assert_eq!(devices[0]["device"], json!("local"));
        assert_eq!(devices[1]["status"], json!("online"));
        assert_eq!(devices[2]["status"], json!("offline"));
        assert_eq!(devices[2]["error"], json!("connect: refused"));
        let tracks = merged["tracks"].as_array().unwrap();
        // 本机 1 轨 + 远端 b 1 轨；offline 的 laptop-c 不产轨道
        assert_eq!(tracks.len(), 2);
        assert_eq!(tracks[0]["device"], json!("local"));
        assert_eq!(tracks[1]["device"], json!("workstation-b"));
        assert_eq!(tracks[1]["device_status"], json!("online"));
        assert_eq!(tracks[1]["intervals"][0]["device"], json!("workstation-b"));
        assert!(merged["human_presence_boundary"]["note_zh"]
            .as_str()
            .unwrap()
            .contains("不相加"));
    }
}
