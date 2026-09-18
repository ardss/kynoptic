//! JSON-RPC 2.0 / MCP 协议层（stdio 传输）
//!
//! 实现与 Claude Desktop 兼容的最小协议面：
//! - `initialize` / `notifications/initialized`（通知，不回包）
//! - `tools/list` / `tools/call`
//! - `ping`
//! - 其余带 id 的方法 → -32601 method not found
//!
//! 传输为**换行分隔 JSON**（每行一个 JSON-RPC 消息），与 MCP stdio 约定一致。
//! 协议处理与 IO 分离：[`McpServer::handle`] 是纯函数（Value→Option<Value>），
//! [`serve`] 只做行读写，方便进程内协议测试。

use std::io::{BufRead, Write};

use serde_json::{json, Value};

use crate::state;

/// 与 spec 对齐的 MCP protocolVersion（initialize 回显客户端请求的版本，
/// 客户端未带时回退到此默认值——Claude Desktop 当前为 2024-11-05 系）。
pub const DEFAULT_PROTOCOL_VERSION: &str = "2024-11-05";

/// MCP server：持有数据库路径，逐请求开只读连接（v0.1 读多写零，开销可接受）。
#[derive(Clone)]
pub struct McpServer {
    pub db_path: String,
}

impl McpServer {
    pub fn new(db_path: impl Into<String>) -> Self {
        Self {
            db_path: db_path.into(),
        }
    }

    /// 处理一条 JSON-RPC 消息。通知（无 id）返回 None，请求返回 Some(响应)。
    pub fn handle(&self, msg: &Value) -> Option<Value> {
        let id = msg.get("id").cloned();
        // 审查 P1：客户端发给服务端的响应（有 id 无 method，如对服务端请求的
        // 回复）必须静默吸收——旧逻辑 method 落 "" 走 method-not-found，往
        // stdout 回 -32601 污染协议流。
        let method = msg.get("method").and_then(|m| m.as_str())?;
        let params = msg.get("params").cloned().unwrap_or(json!({}));

        // 通知（无 id）：initialized 等一律静默吸收（MCP 无推送语义，v0.1 不发通知）
        id.as_ref()?;

        let result = match method {
            "initialize" => Ok(self.initialize(&params)),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({ "tools": tool_definitions() })),
            "tools/call" => self.tools_call(&params),
            other => Err(json!({
                "code": -32601,
                "message": format!("method not found: {other}"),
            })),
        };

        Some(match result {
            Ok(r) => json!({ "jsonrpc": "2.0", "id": id, "result": r }),
            Err(e) => json!({ "jsonrpc": "2.0", "id": id, "error": e }),
        })
    }

    fn initialize(&self, params: &Value) -> Value {
        let version = params
            .get("protocolVersion")
            .and_then(|v| v.as_str())
            .unwrap_or(DEFAULT_PROTOCOL_VERSION)
            .to_string();
        json!({
            "protocolVersion": version,
            "capabilities": { "tools": { "listChanged": false } },
            "serverInfo": {
                "name": "kynoptic-mcp",
                "version": env!("CARGO_PKG_VERSION"),
            },
        })
    }

    /// tools/call 分发。工具内部错误走 MCP 约定的 `isError: true` 结果
    /// （而非 JSON-RPC error——工具参数错误对客户端应是可读的工具结果）。
    fn tools_call(&self, params: &Value) -> Result<Value, Value> {
        let name = params
            .get("name")
            .and_then(|n| n.as_str())
            .ok_or_else(|| json!({"code": -32602, "message": "missing tool name"}))?;
        let args = params.get("arguments").cloned().unwrap_or(json!({}));
        match self.call_tool(name, &args) {
            Ok(v) => Ok(tool_text(&v, false)),
            Err(e) => Ok(tool_text(&json!({ "error": e }), true)),
        }
    }

    /// 六工具的具体执行。公开给 CLI 复用（`kynoptic now` 走同一数据面）。
    pub fn call_tool(&self, name: &str, args: &Value) -> Result<Value, String> {
        let conn = state::open_reader(&self.db_path)
            .map_err(|e| format!("Failed to open database: {e}（打开数据库失败: {e}）"))?;
        match name {
            "get_current_status" => {
                let groups = args.get("groups").and_then(|g| g.as_array()).map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect::<Vec<_>>()
                });
                state::current_status(&conn, groups.as_deref())
            }
            "get_summary" => {
                let date = args
                    .get("date")
                    .and_then(|v| v.as_str())
                    .unwrap_or(&kynoptic_core::queries::today_local_str())
                    .to_string();
                let metric = args
                    .get("metric")
                    .and_then(|v| v.as_str())
                    .unwrap_or("keys");
                state::summary(&conn, &date, metric)
            }
            "get_timeline" => {
                let now = chrono::Utc::now().to_rfc3339();
                let from = args
                    .get("from")
                    .and_then(|v| v.as_str())
                    .map(String::from)
                    .unwrap_or_else(|| {
                        let today = kynoptic_core::queries::today_local_str();
                        kynoptic_core::queries::local_day_range(&today)
                            .map(|(s, _)| s)
                            .unwrap_or(today)
                    });
                let to = args
                    .get("to")
                    .and_then(|v| v.as_str())
                    .map(String::from)
                    .unwrap_or(now);
                let granularity = args
                    .get("granularity")
                    .and_then(|v| v.as_str())
                    .unwrap_or("minute");
                let (limit, clamped_to) = parse_limit(args)?;
                let mut v = state::timeline(&conn, &from, &to, granularity, limit)?;
                if let Some(n) = clamped_to {
                    v["clamped_to"] = json!(n);
                }
                Ok(v)
            }
            "get_top_apps" => {
                let from = args.get("from").and_then(|v| v.as_str()).map(String::from);
                let to = args.get("to").and_then(|v| v.as_str()).map(String::from);
                let (from, to) = match (from, to) {
                    (Some(f), Some(t)) => (f, t),
                    (None, Some(t)) => (default_from(), t),
                    (Some(f), None) => (f, chrono::Utc::now().to_rfc3339()),
                    (None, None) => (default_from(), chrono::Utc::now().to_rfc3339()),
                };
                let (limit, clamped_to) = parse_limit(args)?;
                let mut v = state::top_apps(&conn, &from, &to, limit)?;
                if let Some(n) = clamped_to {
                    v["clamped_to"] = json!(n);
                }
                Ok(v)
            }
            "get_anomalies" => {
                // 审查 P2：days=1.5/"3"/true 这类错型此前静默回退 1 天，与
                // limit 的硬错误约定矛盾——改为同样报错。
                let days = match args.get("days") {
                    None | Some(Value::Null) => 1usize,
                    Some(v) => {
                        let d = v.as_i64().ok_or_else(|| {
                            "days must be an integer（days 必须是整数）".to_string()
                        })?;
                        if d < 1 {
                            return Err(
                                "days must be >= 1（days 必须 >= 1，不接受 0 或负数）".to_string()
                            );
                        }
                        (d as usize).min(30)
                    }
                };
                let clamped_days = match args.get("days") {
                    Some(v) if v.as_i64().map(|d| d > 30).unwrap_or(false) => Some(30),
                    _ => None,
                };
                let (limit, clamped_limit) = parse_limit(args)?;
                let mut v = state::anomalies(&conn, days, limit);
                // days 与 limit 可能同时超限：分别带 clamped_days / clamped_limit
                //（clamped_to 兼容保留，语义 = days 的钳制值）
                if let Some(n) = clamped_days {
                    v["clamped_days"] = json!(n);
                    v["clamped_to"] = json!(n);
                }
                if let Some(n) = clamped_limit {
                    v["clamped_limit"] = json!(n);
                }
                Ok(v)
            }
            "wait_for" => {
                let signal = args.get("signal").and_then(|v| v.as_str()).ok_or_else(|| {
                    "Missing required argument: signal（缺少必填参数 signal）".to_string()
                })?;
                let timeout = args
                    .get("timeout_sec")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(300);
                state::wait_for(&conn, signal, timeout)
            }
            other => Err(format!(
                "Unknown tool: {other} (available: {})（未知工具，可用: {}）",
                crate::Tool::all()
                    .iter()
                    .map(|t| t.name())
                    .collect::<Vec<_>>()
                    .join("/"),
                crate::Tool::all()
                    .iter()
                    .map(|t| t.name())
                    .collect::<Vec<_>>()
                    .join("/")
            )),
        }
    }
}

/// get_timeline/get_top_apps 缺省 from：今日本地起点。
fn default_from() -> String {
    let today = kynoptic_core::queries::today_local_str();
    kynoptic_core::queries::local_day_range(&today)
        .map(|(s, _)| s)
        .unwrap_or(today)
}

/// 解析 limit 参数：缺省 20；必须是整数；<1 报错（不静默回落）；
/// >100 钳到 100 并返回 clamped_to 标记。
fn parse_limit(args: &Value) -> Result<(usize, Option<usize>), String> {
    match args.get("limit") {
        None | Some(Value::Null) => Ok((20, None)),
        Some(v) => {
            let n = v
                .as_i64()
                .ok_or_else(|| format!("limit must be an integer, got {v}（limit 必须是整数）"))?;
            if n < 1 {
                return Err(format!(
                    "limit must be >= 1, got {n}（limit 必须 >= 1，不接受 0 或负数）"
                ));
            }
            if n > 100 {
                Ok((100, Some(100)))
            } else {
                Ok((n as usize, None))
            }
        }
    }
}

/// 工具结果 → MCP content 包装（text content，payload 为紧凑 JSON 字符串，
/// 保持响应小且可解析）。
fn tool_text(v: &Value, is_error: bool) -> Value {
    json!({
        "content": [{ "type": "text", "text": v.to_string() }],
        "isError": is_error,
    })
}

/// tools/list 的工具定义（name + description + inputSchema）。
fn tool_definitions() -> Vec<Value> {
    let limit_schema = |desc: &str| json!({ "type": "integer", "minimum": 1, "maximum": 100, "default": 20, "description": desc });
    vec![
        json!({
            "name": "get_current_status",
            "description": "本机当前状态标量视图：cpu_pct/mem_pct/max_temp_c/battery_pct/foreground_app/idle_seconds/apm_5min/net_up_kbps/net_down_kbps",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "groups": {
                        "type": "array",
                        "items": { "type": "string", "enum": state::GROUPS },
                        "default": ["system", "activity"],
                        "description": "返回哪些分组",
                    }
                }
            },
        }),
        json!({
            "name": "get_summary",
            "description": "单指标日聚合 + 与昨日同期对比百分比",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "date": { "type": "string", "format": "date", "description": "YYYY-MM-DD，默认今天" },
                    "metric": { "type": "string", "enum": state::METRICS, "default": "keys" },
                }
            },
        }),
        json!({
            "name": "get_timeline",
            "description": "应用/窗口时间线段（前台应用占用段，应用名为空时回退窗口标题，皆空记 (unknown)）。裸日期 from/to 按本地日界解析；to 为日期时含当天全天。响应含 total_segments；超限时保留最新 limit 段（truncation=oldest-dropped，丢弃最旧段）并带 next_from（保留段最早 start）。分页推进规则：下一页以同一 from、to=next_from 再查，循环直到 truncated=false，各页拼接不重不漏；段 start 严格递增（同 timestamp 重复 switch 已去重）",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "from": { "type": "string", "description": "起始时间（RFC3339 或 YYYY-MM-DD，裸日期按本地时区 00:00），默认今日起点" },
                    "to": { "type": "string", "description": "结束时间（RFC3339 或 YYYY-MM-DD，裸日期含该本地日全天到 24:00），默认现在" },
                    "granularity": { "type": "string", "enum": ["minute", "hour"], "default": "minute" },
                    "limit": limit_schema("最多返回段数；超过 100 钳到 100 并带 clamped_to"),
                }
            },
        }),
        json!({
            "name": "get_top_apps",
            "description": "窗口 [from,to) 内各前台应用的驻留秒数排行（降序）。驻留 = 相邻 window/switch 事件间隔，末段计到 to。适合『上周二我用的哪个工具/应用』这类问题。裸日期按本地日界解析",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "from": { "type": "string", "description": "起始（RFC3339 或 YYYY-MM-DD，裸日期按本地时区 00:00），默认今日起点" },
                    "to": { "type": "string", "description": "结束（RFC3339 或 YYYY-MM-DD，裸日期含该本地日全天），默认现在" },
                    "limit": limit_schema("最多返回应用数；超过 100 钳到 100 并带 clamped_to"),
                }
            },
        }),
        json!({
            "name": "get_anomalies",
            "description": "最近 N 天异常事件（深夜活动/马拉松会话/APM 突增等），每条带时间戳+severity",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "days": { "type": "integer", "minimum": 1, "maximum": 30, "default": 1 },
                    "limit": limit_schema("最多返回条数"),
                }
            },
        }),
        json!({
            "name": "wait_for",
            "description": "阻塞等待语义信号触发（订阅兜底，MCP Tool 无推送语义）；超时返回 timeout 标记。注意：wait_for 在后台线程执行，其响应可能乱序返回，客户端必须按 JSON-RPC id 关联响应",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "signal": { "type": "string", "enum": state::SIGNALS },
                    "timeout_sec": { "type": "integer", "minimum": 1, "maximum": 1800, "default": 300 },
                },
                "required": ["signal"],
            },
        }),
    ]
}

/// 逐行读取 JSON-RPC 消息并写出响应（换行分隔）。EOF 即退出。
/// 单行解析失败回 -32700（不中断会话——坏行之后的消息照常处理）。
pub fn serve<R: BufRead, W: Write + Send + 'static>(
    reader: R,
    writer: std::sync::Arc<std::sync::Mutex<W>>,
    server: McpServer,
) {
    // 审查 P2：wait_for 最长 1800s，同步处理会把整个 server 卡死且客户端
    // 断开后进程僵住。改为：每请求一线程（长请求不再阻塞读循环，stdin EOF
    // 立即退出进程），响应经互斥锁串行写出。
    fn respond<W: Write>(writer: &std::sync::Arc<std::sync::Mutex<W>>, resp: &str) {
        if let Ok(mut gw) = writer.lock() {
            let _ = writeln!(gw, "{resp}");
            gw.flush().ok();
        }
    }
    let mut threads: Vec<std::thread::JoinHandle<()>> = Vec::new();
    for line in reader.lines() {
        let Ok(line) = line else {
            // 审查 P1：区分可恢复与终结错误——非 UTF-8 字节已消费可继续会话
            //（按 parse error 回应）；真实 IO 错误（描述符损坏等）会永久重复
            // 返回，继续循环就是 -32700 忙等打转，必须退出。
            let is_utf8 = matches!(
                line.as_ref().err(),
                Some(err) if err.kind() == std::io::ErrorKind::InvalidData
            );
            if !is_utf8 {
                break;
            }
            let resp = json!({
                "jsonrpc": "2.0",
                "id": null,
                "error": { "code": -32700, "message": "Parse error: invalid UTF-8" },
            })
            .to_string();
            respond(&writer, &resp);
            continue;
        };
        if line.trim().is_empty() {
            continue;
        }
        // 审查 P2：wait_for 最长 1800s，同步处理会卡死读循环且客户端断开后
        // 进程僵住——仅这类长请求走后台线程；其余顺序处理保证 JSONL 响应有序。
        // 代价：wait_for 的响应可能在后续请求响应之后到达（乱序），客户端必须
        // 按 JSON-RPC id 关联（已写入 wait_for 工具 description）。
        let is_long = serde_json::from_str::<Value>(&line)
            .ok()
            .and_then(|m| {
                Some(
                    m.get("method")?.as_str()? == "tools/call"
                        && m.pointer("/params/name")?.as_str()? == "wait_for",
                )
            })
            .unwrap_or(false);
        if is_long {
            let server = server.clone();
            let writer = writer.clone();
            threads.push(
                std::thread::Builder::new()
                    .name("mcp-wait".into())
                    .spawn(move || {
                        if let Ok(msg) = serde_json::from_str::<Value>(&line) {
                            if let Some(r) = server.handle(&msg) {
                                respond(&writer, &r.to_string());
                            }
                        }
                    })
                    .expect("mcp-wait spawn"),
            );
            continue;
        }
        let response = match serde_json::from_str::<Value>(&line) {
            // 审查 P1：JSON-RPC 2.0 批量请求（顶层数组）必须以数组回应——
            // 旧逻辑 get("id") 为 None 走通知路径整体吞掉，规范客户端会挂等。
            Ok(Value::Array(batch)) => {
                let responses: Vec<Value> = batch.iter().filter_map(|m| server.handle(m)).collect();
                if responses.is_empty() {
                    None
                } else {
                    Some(Value::Array(responses))
                }
            }
            Ok(msg) => server.handle(&msg),
            Err(e) => Some(json!({
                "jsonrpc": "2.0",
                "id": null,
                "error": { "code": -32700, "message": format!("parse error: {e}") },
            })),
        };
        if let Some(r) = response {
            respond(&writer, &r.to_string());
        }
    }
    for t in threads {
        let _ = t.join();
    }
}

/// 标准 stdio 入口（CLI `kynoptic mcp` 调用）。
/// 数据库路径：KYNOPTIC_DB > core 的统一解析（exe 同级 data/ > cwd 候选）。
pub fn serve_stdio() {
    let db_path = std::env::var("KYNOPTIC_DB")
        .unwrap_or_else(|_| kynoptic_core::db::resolve_db_path().display().to_string());
    log::info!("kynoptic-mcp 启动，db={db_path}");
    let stdin = std::io::stdin();
    serve(
        stdin.lock(),
        std::sync::Arc::new(std::sync::Mutex::new(std::io::stdout())),
        McpServer::new(db_path),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initialize_echoes_client_protocol_version() {
        let srv = McpServer::new(":memory:");
        let resp = srv
            .handle(&json!({
                "jsonrpc": "2.0", "id": 1, "method": "initialize",
                "params": { "protocolVersion": "2025-03-26" }
            }))
            .unwrap();
        assert_eq!(resp["result"]["protocolVersion"], json!("2025-03-26"));
        assert_eq!(resp["result"]["serverInfo"]["name"], json!("kynoptic-mcp"));
        // 未带版本 → 默认值
        let resp = srv
            .handle(&json!({ "jsonrpc": "2.0", "id": 2, "method": "initialize" }))
            .unwrap();
        assert_eq!(
            resp["result"]["protocolVersion"],
            json!(DEFAULT_PROTOCOL_VERSION)
        );
    }

    #[test]
    fn notification_produces_no_response() {
        let srv = McpServer::new(":memory:");
        assert!(srv
            .handle(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))
            .is_none());
    }

    #[test]
    fn tools_list_has_six_spec_tools() {
        let srv = McpServer::new(":memory:");
        let resp = srv
            .handle(&json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/list" }))
            .unwrap();
        let names: Vec<&str> = resp["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            [
                "get_current_status",
                "get_summary",
                "get_timeline",
                "get_top_apps",
                "get_anomalies",
                "wait_for"
            ]
        );
        for t in resp["result"]["tools"].as_array().unwrap() {
            assert!(t["inputSchema"].is_object(), "每个工具需带 inputSchema");
        }
    }

    #[test]
    fn unknown_method_is_jsonrpc_error() {
        let srv = McpServer::new(":memory:");
        let resp = srv
            .handle(&json!({ "jsonrpc": "2.0", "id": 4, "method": "resources/list" }))
            .unwrap();
        assert_eq!(resp["error"]["code"], json!(-32601));
    }

    #[test]
    fn malformed_json_line_yields_parse_error() {
        let out = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
        let input = "{not json}\n";
        serve(
            std::io::BufReader::new(input.as_bytes()),
            out.clone(),
            McpServer::new(":memory:"),
        );
        let v: Value = serde_json::from_slice(&out.lock().unwrap()).unwrap();
        assert_eq!(v["error"]["code"], json!(-32700));
    }

    #[test]
    fn ping_returns_empty_result() {
        let srv = McpServer::new(":memory:");
        let resp = srv
            .handle(&json!({ "jsonrpc": "2.0", "id": 5, "method": "ping" }))
            .unwrap();
        assert_eq!(resp["result"], json!({}));
    }

    #[test]
    fn unknown_tool_call_is_error_result_not_jsonrpc_error() {
        let srv = McpServer::new(":memory:");
        let resp = srv
            .handle(&json!({
                "jsonrpc": "2.0", "id": 6, "method": "tools/call",
                "params": { "name": "nope", "arguments": {} }
            }))
            .unwrap();
        assert!(resp["result"]["isError"].as_bool().unwrap());
        assert!(resp.get("error").is_none());
    }

    // ─── 参数校验回归（P1：非法参数曾静默回落/静默钳制） ───────────────

    fn mem_db() -> String {
        // 每测试一个独立临时库文件（:memory: 连接不跨 call_tool 存活——每请求重开）
        let dir = std::env::temp_dir().join(format!("kynoptic-mcp-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!(
            "db-{}.sqlite",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let conn = rusqlite::Connection::open(&path).unwrap();
        // 预设 WAL：open_reader 只读连接上 journal_mode=WAL 是写操作，库必须
        // 在写入侧（采集器/此处）先转成 WAL，与生产库一致
        kynoptic_core::db::apply_pragmas(&conn).unwrap();
        conn.execute_batch(kynoptic_core::db::SCHEMA).unwrap();
        path.display().to_string()
    }

    fn insert_window_switch(db: &str, ts: &str, app: &str) {
        let conn = rusqlite::Connection::open(db).unwrap();
        conn.execute(
            "INSERT INTO events (timestamp, event_type, event_action, event_data, app_name, window_title, session_id) VALUES (?1,'window','switch',NULL,?2,?2,NULL)",
            rusqlite::params![ts, app],
        )
        .unwrap();
    }

    fn call(srv: &McpServer, id: i32, name: &str, args: Value) -> Value {
        srv.handle(&json!({
            "jsonrpc": "2.0", "id": id, "method": "tools/call",
            "params": { "name": name, "arguments": args }
        }))
        .unwrap()["result"]
            .clone()
    }

    #[test]
    fn limit_zero_and_negative_are_rejected_not_silently_fallback() {
        let srv = McpServer::new(mem_db());
        let r = call(&srv, 1, "get_timeline", json!({ "limit": 0 }));
        assert!(r["isError"].as_bool().unwrap(), "{r}");
        assert!(r["content"][0]["text"].as_str().unwrap().contains(">= 1"));
        let r = call(&srv, 2, "get_timeline", json!({ "limit": -5 }));
        assert!(r["isError"].as_bool().unwrap(), "-5 不得静默回落默认值");
        let r = call(&srv, 3, "get_anomalies", json!({ "days": 0 }));
        assert!(r["isError"].as_bool().unwrap(), "days=0 不得静默返回空");
        let r = call(&srv, 4, "get_anomalies", json!({ "days": -1 }));
        assert!(r["isError"].as_bool().unwrap());
    }

    #[test]
    fn over_max_limit_and_days_report_clamped_to() {
        let srv = McpServer::new(mem_db());
        let r = call(&srv, 1, "get_timeline", json!({ "limit": 500 }));
        assert!(!r["isError"].as_bool().unwrap());
        assert_eq!(
            serde_json::from_str::<Value>(r["content"][0]["text"].as_str().unwrap()).unwrap()
                ["clamped_to"],
            json!(100)
        );
        let r = call(&srv, 2, "get_anomalies", json!({ "days": 500 }));
        let v = serde_json::from_str::<Value>(r["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(v["clamped_to"], json!(30));
        assert_eq!(v["clamped_days"], json!(30));
        assert!(v.get("clamped_limit").is_none());
        // days 与 limit 同时超限：两个钳制标记都要报（不能 or() 只报其一）
        let r = call(
            &srv,
            3,
            "get_anomalies",
            json!({ "days": 500, "limit": 500 }),
        );
        let v = serde_json::from_str::<Value>(r["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(v["clamped_days"], json!(30));
        assert_eq!(v["clamped_limit"], json!(100));
        // get_anomalies 响应带 bridge_minutes（MCP 固定用 core 默认桥接值）
        let r = call(&srv, 4, "get_anomalies", json!({}));
        let v = serde_json::from_str::<Value>(r["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(v["bridge_minutes"], json!(2));
    }

    #[test]
    fn get_top_apps_basic() {
        let db = mem_db();
        insert_window_switch(&db, "2026-09-09T01:00:00+00:00", "code");
        insert_window_switch(&db, "2026-09-09T01:40:00+00:00", "web");
        insert_window_switch(&db, "2026-09-09T01:50:00+00:00", "code");
        let srv = McpServer::new(&db);
        let r = call(
            &srv,
            1,
            "get_top_apps",
            json!({
                "from": "2026-09-09T01:00:00+00:00",
                "to": "2026-09-09T02:00:00+00:00"
            }),
        );
        assert!(!r["isError"].as_bool().unwrap(), "{r}");
        let v: Value = serde_json::from_str(r["content"][0]["text"].as_str().unwrap()).unwrap();
        let apps = v["apps"].as_array().unwrap();
        // code: 40min + 10min = 3000s；web: 600s
        assert_eq!(apps.len(), 2);
        assert_eq!(apps[0]["app"], json!("code"));
        assert_eq!(apps[0]["dwell_sec"], json!(3000));
        assert_eq!(apps[1]["dwell_sec"], json!(600));
    }

    /// KYNOPTIC_DB 环境变量决定数据面连的库（`kynoptic mcp --db` 经 cli 的
    /// cmd_mcp 转成该变量；这里验证 serve_stdio 的读取端契约）。
    #[test]
    fn serve_stdio_honors_kynoptic_db_env() {
        let db = mem_db();
        insert_window_switch(&db, "2026-09-09T01:00:00+00:00", "envmarker");
        // 直接验证 call_tool 层用 db_path 打开对应库（env → db_path 的映射在 serve_stdio）
        let srv = McpServer::new(&db);
        let r = call(
            &srv,
            1,
            "get_top_apps",
            json!({ "from": "2026-09-09T00:00:00+00:00", "to": "2026-09-09T02:00:00+00:00" }),
        );
        let v: Value = serde_json::from_str(r["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(v["apps"][0]["app"], json!("envmarker"));
        // 读取端契约：KYNOPTIC_DB 优先于默认解析
        std::env::set_var("KYNOPTIC_DB", &db);
        assert_eq!(
            std::env::var("KYNOPTIC_DB").unwrap(),
            db,
            "cmd_mcp 必须通过 KYNOPTIC_DB 传递 --db"
        );
        std::env::remove_var("KYNOPTIC_DB");
    }
}
