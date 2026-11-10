//! MCP 协议层集成测试 —— 进程内 spawn + 内存 stdio 全往返
//!
//! 不起真实子进程：把请求行喂给 [`kynoptic_mcp::server::serve`] 的字节流，
//! 捕获其 stdout 输出，按行断言 initialize / tools/list / tools/call 往返。

use std::io::BufReader;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use chrono::Utc;
use kynoptic_core::db::Database;
use kynoptic_core::types::{Event, EventAction, EventType};
use kynoptic_mcp::McpServer;
use rusqlite::params;
use serde_json::{json, Value};

static SEQ: AtomicU64 = AtomicU64::new(0);

fn seeded_db() -> PathBuf {
    let seq = SEQ.fetch_add(1, Ordering::SeqCst);
    let mut p = std::env::temp_dir();
    p.push(format!("kyn_mcp_test_{}_{}.db", std::process::id(), seq));
    let db = Database::open(p.to_str().unwrap()).expect("seed db");
    let mut events: Vec<Event> = Vec::new();
    let now = Utc::now().to_rfc3339();

    let push = |e: &mut Vec<Event>,
                ts: &str,
                t: EventType,
                a: EventAction,
                app: Option<&str>,
                data: Value| {
        let mut ev = Event::new(a, t);
        ev.timestamp = ts.to_string();
        ev.app_name = app.map(String::from);
        ev.window_title = app.map(String::from);
        ev.event_data = Some(data);
        e.push(ev);
    };
    push(
        &mut events,
        &now,
        EventType::Keyboard,
        EventAction::Press,
        None,
        json!({}),
    );
    push(
        &mut events,
        &now,
        EventType::Keyboard,
        EventAction::Press,
        None,
        json!({}),
    );
    push(
        &mut events,
        &now,
        EventType::Mouse,
        EventAction::Click,
        None,
        json!({}),
    );
    push(
        &mut events,
        &now,
        EventType::Window,
        EventAction::Switch,
        Some("vscode"),
        json!({}),
    );
    push(
        &mut events,
        &now,
        EventType::System,
        EventAction::Heartbeat,
        None,
        json!({"cpu_percent": 12.0, "memory": {"used_percent": 55.0}}),
    );
    db.insert_events(&events);
    p
}

fn cleanup(path: &PathBuf) {
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(format!("{}-wal", path.display()));
    let _ = std::fs::remove_file(format!("{}-shm", path.display()));
}

/// 请求行 → serve → 响应行数组
fn roundtrip(db_path: &str, requests: &[Value]) -> Vec<Value> {
    let mut input = String::new();
    for r in requests {
        input.push_str(&r.to_string());
        input.push('\n');
    }
    let out = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
    kynoptic_mcp::server::serve(
        BufReader::new(input.as_bytes()),
        out.clone(),
        McpServer::new(db_path),
    );
    let out = std::sync::Arc::try_unwrap(out)
        .ok()
        .map(|m| m.into_inner().unwrap_or_default())
        .unwrap_or_default();
    String::from_utf8(out)
        .expect("输出应为 UTF-8")
        .lines()
        .map(|l| serde_json::from_str(l).expect("每行应为合法 JSON"))
        .collect()
}

fn text_payload(resp: &Value) -> Value {
    let text = resp["result"]["content"][0]["text"]
        .as_str()
        .expect("text content");
    serde_json::from_str(text).expect("payload 应为 JSON")
}

#[test]
fn full_initialize_list_call_roundtrip() {
    let path = seeded_db();
    let db_path = path.to_str().unwrap().to_string();
    let responses = roundtrip(
        &db_path,
        &[
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05"}}),
            json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
            json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
            json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"get_current_status","arguments":{"groups":["system","activity"]}}}),
            json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"get_summary","arguments":{"metric":"keys"}}}),
            json!({"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"get_anomalies","arguments":{}}}),
        ],
    );
    // 通知不回包：6 条请求（1 条通知）→ 5 条响应
    assert_eq!(responses.len(), 5);
    assert_eq!(
        responses[0]["result"]["protocolVersion"],
        json!("2024-11-05")
    );
    let tools = responses[1]["result"]["tools"].as_array().unwrap();
    // Stage 3 新增 get_unified_timeline / get_reports（PRD F07/F08）：九工具
    assert_eq!(tools.len(), 9);

    let status = text_payload(&responses[2]);
    assert_eq!(status["cpu_pct"], json!(12.0));
    assert_eq!(status["foreground_app"], json!("vscode"));

    let summary = text_payload(&responses[3]);
    assert_eq!(summary["metric"], json!("keys"));
    assert_eq!(summary["value"], json!(2.0));

    let anomalies = text_payload(&responses[4]);
    assert!(anomalies["anomalies"].is_array());
    cleanup(&path);
}

/// PRD v1.0 Stage 3（F07/F08/F11）：get_unified_timeline / get_reports 经
/// tools/call 可用；now_ms 显式注入时钟（测试零真实时钟依赖）；响应与
/// dash 端点同一纯函数产出——endpoint 标识、device_boundary 边界说明一致。
#[test]
fn tools_call_unified_timeline_and_reports() {
    let path = seeded_db();
    let db_path = path.to_str().unwrap().to_string();
    // 固定 now：2026-09-01T02:00:00Z = 1786442400000ms（不读真实时钟）
    let now_ms: i64 = 1_786_442_400_000;
    let responses = roundtrip(
        &db_path,
        &[
            json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"get_unified_timeline","arguments":{"minutes":60,"now_ms":now_ms}}}),
            json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"get_reports","arguments":{"days":7,"now_ms":now_ms}}}),
            json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"get_unified_timeline","arguments":{"minutes":0,"now_ms":now_ms}}}),
        ],
    );
    let tl = text_payload(&responses[0]);
    assert_eq!(tl["endpoint"], json!("unified_timeline"));
    assert_eq!(tl["minutes"], json!(60));
    assert_eq!(tl["device_boundary"]["scope"], json!("this_database"));
    assert!(tl["tracks"].is_array());

    let rp = text_payload(&responses[1]);
    assert_eq!(rp["endpoint"], json!("reports"));
    assert_eq!(rp["days"], json!(7));
    assert_eq!(rp["device_boundary"]["scope"], json!("this_database"));
    assert!(rp["rows"].is_array());
    assert!(rp["devices"].is_array());

    // minutes=0 硬错误（不静默回落默认，与 days/limit 同政策）
    let err = text_payload(&responses[2]);
    assert_eq!(
        err["error"],
        json!("minutes must be >= 1（minutes 必须 >= 1，不接受 0 或负数）")
    );
    cleanup(&path);
}

#[test]
fn tools_call_timeline_and_limit_clamping() {
    let path = seeded_db();
    let db_path = path.to_str().unwrap().to_string();
    // 额外补 2 条 window/switch（同一天不同时间点）供时间线测试
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    for ts in ["2026-09-01T01:00:00+00:00", "2026-09-01T01:10:00+00:00"] {
        conn.execute(
            "INSERT INTO events (timestamp, event_type, event_action, event_data, app_name, window_title, session_id) VALUES (?1,'window','switch',NULL,?2,?2,NULL)",
            params![ts, "app-x"],
        )
        .unwrap();
    }
    drop(conn);

    let responses = roundtrip(
        &db_path,
        &[
            json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"get_timeline","arguments":{
                "from":"2026-09-01T00:00:00+00:00","to":"2026-09-01T02:00:00+00:00","limit":10000}}}),
            json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"wait_for","arguments":{"signal":"thermal_hot","timeout_sec":1}}}),
        ],
    );
    // limit=10000 被钳到 100，2 段 < 100 所以不截断
    let timeline = text_payload(&responses[0]);
    assert_eq!(timeline["truncated"], json!(false));
    assert_eq!(timeline["segments"].as_array().unwrap().len(), 2);

    // wait_for 超时语义。审查 P2：EOF 后 serve 置位关停信号，后台线程可能在
    // 首轮询点前观察到而以 status=shutdown 立即退出——两种状态都合法，
    // 但都必须带 signal 回显。
    let wf = text_payload(&responses[1]);
    assert!(
        wf["status"] == json!("timeout") || wf["status"] == json!("shutdown"),
        "{wf}"
    );
    assert_eq!(wf["signal"], json!("thermal_hot"));
    cleanup(&path);
}

#[test]
fn tools_call_unknown_tool_is_error_result() {
    let path = seeded_db();
    let responses = roundtrip(
        path.to_str().unwrap(),
        &[json!({"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"nope"}})],
    );
    assert!(responses[0]["result"]["isError"].as_bool().unwrap());
    cleanup(&path);
}
