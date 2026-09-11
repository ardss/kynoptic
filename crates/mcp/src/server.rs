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
        let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
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

    /// 五工具的具体执行。公开给 CLI 复用（`kynoptic now` 走同一数据面）。
    pub fn call_tool(&self, name: &str, args: &Value) -> Result<Value, String> {
        let conn = state::open_reader(&self.db_path).map_err(|e| format!("打开数据库失败: {e}"))?;
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
                let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(20) as usize;
                let (segments, truncated) = state::timeline(&conn, &from, &to, granularity, limit)?;
                Ok(json!({ "segments": segments, "truncated": truncated }))
            }
            "get_anomalies" => {
                let days = args.get("days").and_then(|v| v.as_u64()).unwrap_or(1) as usize;
                let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(20) as usize;
                Ok(state::anomalies(&conn, days, limit))
            }
            "wait_for" => {
                let signal = args
                    .get("signal")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| "缺少 signal 参数".to_string())?;
                let timeout = args
                    .get("timeout_sec")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(300);
                state::wait_for(&conn, signal, timeout)
            }
            other => Err(format!(
                "未知工具: {other}（可用: {}）",
                crate::Tool::all()
                    .iter()
                    .map(|t| t.name())
                    .collect::<Vec<_>>()
                    .join("/")
            )),
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
            "description": "本机当前状态标量视图：cpu_pct/mem_pct/max_temp_c/battery_pct/foreground_app/idle_seconds/apm_5min/net_up/down_kbps",
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
            "description": "应用/窗口时间线段落（前台应用占用段，不含窗口标题全文）",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "from": { "type": "string", "description": "起始时间（RFC3339 或 YYYY-MM-DD），默认今日起点" },
                    "to": { "type": "string", "description": "结束时间，默认现在" },
                    "granularity": { "type": "string", "enum": ["minute", "hour"], "default": "minute" },
                    "limit": limit_schema("最多返回段数"),
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
            "description": "阻塞等待语义信号触发（订阅兜底，MCP Tool 无推送语义）；超时返回 timeout 标记",
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
            // 非 UTF-8 字节：按 parse error 回应并继续会话（审查 P1：
            // 此前直接 break 静默退出，与畸形 JSON 的容错策略自相矛盾）
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
        // 进程僵住——仅这类长请求走后台线程；其余顺序处理保证 JSONL 响应有序
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
    fn tools_list_has_five_spec_tools() {
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
}
