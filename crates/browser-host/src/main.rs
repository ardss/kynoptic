//! kynoptic-browser-host 进程入口：Chrome Native Messaging host。
//!
//! 进程生命周期由 Chrome 管理：`sendNativeMessage` 每次调用拉起一次性进程，
//! 处理完一条消息、写库成功、flush 响应后即退出（browser-extension/
//! native-messaging/README.md 协议 2）。stdout 是协议通道，任何日志只准
//! 走 stderr（env_logger 默认写 stderr）。
//!
//! 退出码约定：
//! - 0  正常（处理完成或干净 EOF）；
//! - 2  消息级错误（解析失败/oversize/不支持类型）——已回写 ok:false 响应；
//! - 1  环境级错误（db 打不开/写库失败/io 错误），无法保证响应已回写。

use std::io::BufReader;
use std::process::ExitCode;

use kynoptic_browser_host::{
    error_response, ingest_ext_event, parse_ext_message, read_message, write_message, ProtoError,
};

fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn"))
        .format_timestamp(None)
        .try_init()
        .ok();

    // 参数极简：可选 --db PATH（默认走 core 的统一路径解析，与主应用同库）。
    let args: Vec<String> = std::env::args().skip(1).collect();
    let db_path = match args.iter().position(|a| a == "--db") {
        Some(i) => match args.get(i + 1) {
            Some(p) => std::path::PathBuf::from(p),
            None => {
                eprintln!("[!] --db 需要路径");
                return ExitCode::from(1);
            }
        },
        None => kynoptic_core::db::resolve_db_path(),
    };

    let db = match kynoptic_core::db::Database::open(db_path.to_str().unwrap_or("?")) {
        Ok(d) => d,
        Err(e) => {
            // 解析层之前的失败按环境错误直接退出（扩展侧会收到端口断开并转缓冲）。
            eprintln!("[!] 数据库打开失败: {e}");
            return ExitCode::from(1);
        }
    };

    let stdin = std::io::stdin();
    let mut reader = BufReader::new(stdin.lock());
    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    let (response, exit) = match read_message(&mut reader) {
        Ok(None) => (None, ExitCode::SUCCESS), // 干净 EOF：正常退出
        Ok(Some(raw)) => match parse_ext_message(&raw) {
            Ok(ev) => {
                let written = db.with_writer(
                    |conn| -> Result<kynoptic_browser_host::IngestOutcome, rusqlite::Error> {
                        let device = kynoptic_browser_host::ensure_device(conn);
                        ingest_ext_event(conn, &ev, device.as_deref())
                    },
                    || {
                        // writer 不可用（库降级/锁恢复中）：显式失败，
                        // 扩展侧按 lastError 转入本地缓冲，稍后重发。
                        Err(rusqlite::Error::InvalidQuery)
                    },
                );
                match written {
                    Ok(o) => (Some(o.to_response().to_string()), ExitCode::SUCCESS),
                    Err(e) => (
                        Some(serde_json::json!({ "ok": false, "error": format!("db write failed: {e}") }).to_string()),
                        ExitCode::from(1),
                    ),
                }
            }
            Err(e @ ProtoError::Oversize(_)) => {
                // oversize：按协议回错误响应后安全退出
                (Some(error_response(&e).to_string()), ExitCode::from(2))
            }
            Err(e) => (Some(error_response(&e).to_string()), ExitCode::from(2)),
        },
        Err(e) => {
            // 半截长度前缀/残缺消息：无法可靠回响应，安全退出
            eprintln!("[!] 协议读取失败: {e}");
            (None, ExitCode::from(2))
        }
    };

    if let Some(body) = response {
        if write_message(&mut out, body.as_bytes()).is_err() {
            eprintln!("[!] 响应回写失败（扩展可能已断开）");
            return ExitCode::from(1);
        }
    }
    exit
}
