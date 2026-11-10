//! Kynoptic browser native messaging host（PRD v1.0 F02）。
//!
//! 职责边界：
//! - Chrome Native Messaging 协议：stdin 读 4 字节小端长度前缀 UTF-8 JSON，
//!   stdout 回同格式 JSON 响应（扩展侧 `sendNativeMessage` 必须收到一条响应，
//!   否则回调挂起——见 browser-extension/native-messaging/README.md 协议 1）。
//! - 扩展事件（`.tab_change` / `.ai_session`）经 core 现有 db 写入路径写
//!   **events 表**：append-only、不建第二真相表（铁律）；幂等去重键——
//!   `.tab_change` 用 `event_data.$.event_id`、`.ai_session` 用
//!   `event_data.$.event_data.session_id`（与 ai_adapter 的 session_id
//!   口径一致），已存在则跳过、绝不删除任何行。
//! - `source_id='chrome_ext'`、`device_id` 走 metadata 表机制（复用
//!   core `ai_adapter::ensure_device_id`，与 AI 适配器同源）。
//! - 入库 timestamp 一律取事件自带的 `ts_ms`（禁止读真实时钟；测试侧
//!   也由 fixture 显式注入 `_at` 风格毫秒值）。
//!
//! 防御性（native-messaging/README.md 协议 7）：解析失败 / 字段缺失 /
//! oversize 一律回 `{"ok": false, "error": ...}` 后由调用方安全退出，
//! 绝不 panic；stdout 是协议通道，日志只准写 stderr。

use std::io::{Read, Write};

/// Native messaging host 名（Chrome 注册表键名 + 扩展
/// background.js 的 NATIVE_HOST_NAME，两端必须一致）。
pub const HOST_NAME: &str = "kynoptic_browser_host";

/// 入站消息上限：Chrome→host 官方上限 1 MiB。超限按协议错误安全退出。
pub const MAX_MESSAGE_BYTES: usize = 1024 * 1024;

/// 长度前缀字节数（u32 小端）。
const LEN_PREFIX_BYTES: usize = 4;

/// 事件来源标识（events.source_id 列）。
pub const SOURCE_ID: &str = "chrome_ext";

/// 消息级错误（不含 io 错误——io 错误无法回写响应，直接退出）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtoError {
    /// 消息长度超过 [`MAX_MESSAGE_BYTES`]。
    Oversize(u32),
    /// 非 UTF-8 JSON。
    NotUtf8,
    /// JSON 解析失败或顶层不是对象。
    BadJson(String),
    /// 必需字段缺失/类型不符（字段路径 + 原因）。
    BadField(String),
    /// 事件类型不在受支持清单内。
    UnsupportedType(String),
}

impl std::fmt::Display for ProtoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProtoError::Oversize(n) => {
                write!(f, "oversize message: {n} bytes > {MAX_MESSAGE_BYTES}")
            }
            ProtoError::NotUtf8 => write!(f, "message is not valid UTF-8"),
            ProtoError::BadJson(e) => write!(f, "invalid JSON: {e}"),
            ProtoError::BadField(w) => write!(f, "missing or invalid field: {w}"),
            ProtoError::UnsupportedType(t) => write!(f, "unsupported event_type: {t}"),
        }
    }
}

/// 单条消息的处理结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IngestOutcome {
    /// 新写入 events 表的行数（0 或 1）。
    pub written: usize,
    /// true = 命中幂等去重（同事件已存在，跳过；未删除任何行）。
    pub deduped: bool,
}

impl IngestOutcome {
    /// 生成回写给扩展的响应对象。
    pub fn to_response(self) -> serde_json::Value {
        serde_json::json!({
            "ok": true,
            "written": self.written,
            "deduped": self.deduped,
            "source": SOURCE_ID,
        })
    }
}

// ─── 协议：4 字节小端长度前缀 ──────────────────────────────────────────

/// 从 `r` 读一条长度前缀消息。流干净关闭（EOF 恰在长度前缀边界）返回
/// `Ok(None)`——host 随之正常退出（0）。
pub fn read_message(r: &mut impl Read) -> std::io::Result<Option<Vec<u8>>> {
    let mut prefix = [0u8; LEN_PREFIX_BYTES];
    let mut filled = 0usize;
    while filled < LEN_PREFIX_BYTES {
        let n = r.read(&mut prefix[filled..])?;
        if n == 0 {
            if filled == 0 {
                return Ok(None); // 干净 EOF
            }
            // 半截长度前缀：管道异常，视为 EOF 但已无法回响应
            return Ok(None);
        }
        filled += n;
    }
    let len = u32::from_le_bytes(prefix);
    if len as usize > MAX_MESSAGE_BYTES {
        // oversize：不能盲目读（可能巨大），交给调用方安全退出
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            ProtoError::Oversize(len).to_string(),
        ));
    }
    let mut buf = vec![0u8; len as usize];
    let mut got = 0usize;
    while got < buf.len() {
        let n = r.read(&mut buf[got..])?;
        if n == 0 {
            // 声明的长度没读到：消息残缺，按解析失败处理
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                ProtoError::BadJson("truncated message body".to_string()).to_string(),
            ));
        }
        got += n;
    }
    Ok(Some(buf))
}

/// 写一条长度前缀消息并 flush。stdout 是协议通道，写失败即 fatal。
pub fn write_message(w: &mut impl Write, payload: &[u8]) -> std::io::Result<()> {
    let len = u32::try_from(payload.len())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "response too large"))?;
    w.write_all(&len.to_le_bytes())?;
    w.write_all(payload)?;
    w.flush()
}

// ─── 解析 ──────────────────────────────────────────────────────────────

/// 解析一条扩展消息为入库所需字段。纯函数（不碰时钟、不碰 db），
/// 供单测与主循环共用。
pub fn parse_ext_message(raw: &[u8]) -> Result<ExtEvent, ProtoError> {
    let text = std::str::from_utf8(raw).map_err(|_| ProtoError::NotUtf8)?;
    let v: serde_json::Value =
        serde_json::from_str(text).map_err(|e| ProtoError::BadJson(e.to_string()))?;
    let obj = v
        .as_object()
        .ok_or_else(|| ProtoError::BadJson("top level is not a JSON object".into()))?;

    let event_type = obj
        .get("event_type")
        .and_then(|x| x.as_str())
        .ok_or_else(|| ProtoError::BadField("event_type (string)".into()))?
        .to_string();
    if event_type != ".tab_change" && event_type != ".ai_session" {
        return Err(ProtoError::UnsupportedType(event_type));
    }
    let ts_ms = obj
        .get("ts_ms")
        .and_then(|x| x.as_i64())
        .ok_or_else(|| ProtoError::BadField("ts_ms (integer milliseconds)".into()))?;
    if ts_ms < 0 {
        return Err(ProtoError::BadField("ts_ms must be >= 0".into()));
    }

    // 去重键：两种事件各自约定（见模块文档）
    let dedup_key = if event_type == ".ai_session" {
        v.pointer("/event_data/session_id")
            .and_then(|x| x.as_str())
            .ok_or_else(|| ProtoError::BadField("event_data.session_id (string)".into()))?
            .to_string()
    } else {
        obj.get("event_id")
            .and_then(|x| x.as_str())
            .ok_or_else(|| ProtoError::BadField("event_id (string, client-generated)".into()))?
            .to_string()
    };

    Ok(ExtEvent {
        event_type,
        ts_ms,
        dedup_key,
        payload: v,
    })
}

/// 解析后的扩展事件（`payload` 是完整消息原文，整包进 event_data）。
#[derive(Debug, Clone)]
pub struct ExtEvent {
    pub event_type: String,
    pub ts_ms: i64,
    pub dedup_key: String,
    pub payload: serde_json::Value,
}

impl ExtEvent {
    /// events 表列映射：
    /// - `.tab_change` → event_type='window' / event_action='tab_change'
    ///   （与现有 window 轨口径一致，charts/timeline 查询按此计数）；
    /// - `.ai_session` → event_type='ai_session' / event_action='ai_run'
    ///   （与 ai_adapter 写入行同构，timeline 的 ai_run 轨直接可见）。
    pub fn table_type(&self) -> (&'static str, &'static str) {
        if self.event_type == ".ai_session" {
            ("ai_session", "ai_run")
        } else {
            ("window", "tab_change")
        }
    }

    /// events.timestamp（RFC3339，UTC +00:00，与 ai_adapter::rfc3339 同格式）。
    pub fn timestamp(&self) -> String {
        chrono::DateTime::from_timestamp_millis(self.ts_ms)
            .unwrap_or(chrono::DateTime::UNIX_EPOCH)
            .to_rfc3339()
    }

    /// 入库 event_data：整包消息原文（url/title 等明细不拆新列）。
    pub fn event_data(&self) -> String {
        self.payload.to_string()
    }
}

// ─── 入库（append-only 幂等）──────────────────────────────────────────

/// 写一条扩展事件进 events 表。append-only：已存在同去重键的行时跳过
/// （返回 deduped=true），绝不 UPDATE/DELETE 任何行。
/// `device_id` 由调用方经 [`ensure_device`] 预先取得（可为 None：事件仍写，
/// 与 ai_adapter 同口径不阻塞）。
///
/// 幂等实现（独立复审 2026-11-11）：单步 `INSERT OR IGNORE`，靠 migration
/// 0019 的两个表达式部分唯一索引（$.event_id / $.event_data.session_id）
/// 在约束层去重。此前的「先 SELECT COUNT 再 INSERT」两步在并发下不原子
/// ——Chrome sendNativeMessage 每次调用拉起独立 host 进程，同键重发可两
/// 进程同时通过 COUNT=0 检查后各插一行；唯一索引把判定下沉到写库原子层，
/// 并发下同样只落一行。注意 INSERT OR IGNORE 也会吞掉其他约束冲突（如
/// NOT NULL）——字段全部由服务端生成且经先验校验，被吞即记 deduped，
/// 如实返回不计入写入。
pub fn ingest_ext_event(
    conn: &rusqlite::Connection,
    ev: &ExtEvent,
    device_id: Option<&str>,
) -> rusqlite::Result<IngestOutcome> {
    let (table_type, table_action) = ev.table_type();
    let n = conn.execute(
        "INSERT OR IGNORE INTO events \
         (timestamp, event_type, event_action, event_data, app_name, window_title, \
          source_id, device_id, quality) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'observed')",
        rusqlite::params![
            ev.timestamp(),
            table_type,
            table_action,
            ev.event_data(),
            "browser",
            ev.payload.get("title").and_then(|t| t.as_str()),
            SOURCE_ID,
            device_id,
        ],
    )?;
    Ok(if n == 1 {
        IngestOutcome {
            written: 1,
            deduped: false,
        }
    } else {
        IngestOutcome {
            written: 0,
            deduped: true,
        }
    })
}

/// device_id 机制与 AI 适配器同源：metadata 表首次生成 UUIDv4。
/// 直接复用 core 的实现，避免第二套真相。
pub fn ensure_device(conn: &rusqlite::Connection) -> Option<String> {
    kynoptic_core::ai_adapter::ensure_device_id(conn)
}

/// 解析失败时的错误响应体。
pub fn error_response(err: &ProtoError) -> serde_json::Value {
    serde_json::json!({ "ok": false, "error": err.to_string() })
}

// ─── 注册：host 清单 + HKCU 注册表 ─────────────────────────────────────

/// 生成填写好的 host 清单 JSON。`extension_ids` 为真机 `chrome://extensions`
/// 显示的 ID（占位符 `EXTENSION_ID_PLACEHOLDER` 必须全部替换后 Chrome 才认）。
pub fn build_host_manifest(host_exe_path: &str, extension_ids: &[&str]) -> serde_json::Value {
    let origins: Vec<String> = extension_ids
        .iter()
        .map(|id| format!("chrome-extension://{id}/"))
        .collect();
    serde_json::json!({
        "name": HOST_NAME,
        "description": "Kynoptic browser native messaging host: receives .tab_change events from the extension and writes them into the kynoptic events table.",
        "path": host_exe_path,
        "type": "stdio",
        "allowed_origins": origins,
    })
}

/// Windows HKCU 注册：把清单文件绝对路径写进
/// `HKCU\Software\Google\Chrome\NativeMessagingHosts\kynoptic_browser_host`
/// 的默认值。HKCU 不需要管理员权限。仅 Windows；其他平台返回不支持错误。
#[cfg(windows)]
pub fn register_windows(manifest_path: &str) -> std::io::Result<()> {
    let key_path = format!(r"Software\Google\Chrome\NativeMessagingHosts\{HOST_NAME}");
    write_hkcu_default_value(&key_path, manifest_path)
}

#[cfg(windows)]
fn write_hkcu_default_value(sub_key: &str, value: &str) -> std::io::Result<()> {
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegCreateKeyExW, RegSetValueExW, HKEY, HKEY_CURRENT_USER, KEY_WRITE,
        REG_OPTION_NON_VOLATILE, REG_SZ,
    };
    let sub_key_w: Vec<u16> = sub_key.encode_utf16().chain(std::iter::once(0)).collect();
    let value_w: Vec<u16> = value.encode_utf16().chain(std::iter::once(0)).collect();
    let mut hkey: HKEY = std::ptr::null_mut();
    let rc = unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            sub_key_w.as_ptr(),
            0,
            std::ptr::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE,
            std::ptr::null(),
            &mut hkey,
            std::ptr::null_mut(),
        )
    };
    if rc != 0 {
        return Err(std::io::Error::from_raw_os_error(rc as i32));
    }
    let rc = unsafe {
        // 默认值名 = null 指针（Windows 注册表默认值）
        RegSetValueExW(
            hkey,
            std::ptr::null(),
            0,
            REG_SZ,
            value_w.as_ptr() as *const u8,
            (value_w.len() * 2) as u32,
        )
    };
    unsafe { RegCloseKey(hkey) };
    if rc != 0 {
        return Err(std::io::Error::from_raw_os_error(rc as i32));
    }
    Ok(())
}

#[cfg(not(windows))]
pub fn register_windows(_manifest_path: &str) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "HKCU registration is Windows-only; on this platform write the manifest to the OS-native native messaging location instead",
    ))
}

// ─── 测试（fixture 全部脱敏；时钟一律注入，禁读真实时钟）───────────────

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    /// 固定注入毫秒时间戳（1_792_977_600_000 = 2026-10-26T01:20:00Z）（仓库测试同基准）。
    const TS_MS: i64 = 1_792_977_600_000;

    fn conn() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(kynoptic_core::db::SCHEMA).unwrap();
        kynoptic_core::db::run_migrations(&c).unwrap();
        c
    }

    /// 脱敏 .tab_change fixture：URL/标题均为样例值，禁用真实站点/路径。
    fn tab_change_json() -> String {
        format!(
            r#"{{"event_type":".tab_change","event_id":"{ts}-activated-42","ts_ms":{ts},
                "kind":"activated","tab_id":42,"window_id":7,
                "url":"https://example.com/sample","title":"Sample Page Title",
                "active":true,"previous_tab_id":41}}"#,
            ts = TS_MS
        )
    }

    #[test]
    fn framing_roundtrip_le_prefix() {
        let payload = br#"{"ok":true}"#;
        let mut wire = Vec::new();
        write_message(&mut wire, payload).unwrap();
        assert_eq!(&wire[..4], &(payload.len() as u32).to_le_bytes());
        let mut cursor = std::io::Cursor::new(wire);
        let got = read_message(&mut cursor).unwrap().unwrap();
        assert_eq!(got, payload.to_vec());
    }

    #[test]
    fn framing_clean_eof_returns_none() {
        let mut empty: &[u8] = &[];
        assert_eq!(read_message(&mut empty).unwrap(), None);
    }

    #[test]
    fn framing_oversize_is_rejected_not_read() {
        let huge = (MAX_MESSAGE_BYTES as u32) + 1;
        let mut wire = huge.to_le_bytes().to_vec();
        wire.extend_from_slice(&[0u8; 8]); // 不会真读这 8 字节
        let mut cursor = std::io::Cursor::new(wire);
        let err = read_message(&mut cursor).unwrap_err();
        assert!(err.to_string().contains("oversize"), "err={err}");
    }

    #[test]
    fn framing_truncated_body_is_error() {
        let mut wire = 10u32.to_le_bytes().to_vec();
        wire.extend_from_slice(b"abc"); // 声明 10 只给 3
        let mut cursor = std::io::Cursor::new(wire);
        assert!(read_message(&mut cursor).is_err());
    }

    #[test]
    fn parse_tab_change_maps_to_window_tab_change() {
        let ev = parse_ext_message(tab_change_json().as_bytes()).unwrap();
        assert_eq!(ev.table_type(), ("window", "tab_change"));
        assert_eq!(ev.ts_ms, TS_MS);
        assert_eq!(
            ev.timestamp(),
            "2026-10-26T01:20:00+00:00",
            "入库时间戳来自事件自带 ts_ms 的注入值，不读真实时钟"
        );
    }

    #[test]
    fn parse_ai_session_dedups_on_session_id() {
        let raw = format!(
            r#"{{"event_type":".ai_session","ts_ms":{ts},"schema_version":1,
                "event_data":{{"schema_version":1,"session_id":"chatgpt_web-tab9-{ts}",
                "source":"chatgpt_web","start":"x","end":"y",
                "start_quality":"observed","end_quality":"observed","prompt_count":null}}}}"#,
            ts = TS_MS
        );
        let ev = parse_ext_message(raw.as_bytes()).unwrap();
        assert_eq!(ev.table_type(), ("ai_session", "ai_run"));
        assert!(ev.dedup_key.starts_with("chatgpt_web-tab9-"));
    }

    #[test]
    fn parse_rejects_bad_input_safely() {
        // 非 JSON
        assert!(matches!(
            parse_ext_message(b"not json"),
            Err(ProtoError::BadJson(_))
        ));
        // JSON 数组顶层
        assert!(matches!(
            parse_ext_message(b"[1]"),
            Err(ProtoError::BadJson(_))
        ));
        // 缺 ts_ms
        assert!(matches!(
            parse_ext_message(br#"{"event_type":".tab_change","event_id":"e1"}"#),
            Err(ProtoError::BadField(_))
        ));
        // 缺 event_id（tab_change）
        let missing_id = format!(r#"{{"event_type":".tab_change","ts_ms":{TS_MS}}}"#);
        assert!(matches!(
            parse_ext_message(missing_id.as_bytes()),
            Err(ProtoError::BadField(_))
        ));
        // 未知事件类型
        let unknown = format!(r#"{{"event_type":".nope","ts_ms":{TS_MS},"event_id":"e"}}"#);
        assert!(matches!(
            parse_ext_message(unknown.as_bytes()),
            Err(ProtoError::UnsupportedType(_))
        ));
        // 响应体统一 ok:false
        assert_eq!(error_response(&ProtoError::NotUtf8)["ok"], false);
    }

    #[test]
    fn ingest_is_append_only_and_idempotent() {
        let c = conn();
        let ev = parse_ext_message(tab_change_json().as_bytes()).unwrap();
        let o1 = ingest_ext_event(&c, &ev, Some("device-test-1")).unwrap();
        assert_eq!(
            o1,
            IngestOutcome {
                written: 1,
                deduped: false
            }
        );

        // 同 event_id 重复投递：跳过，不产生第二行（幂等）
        let o2 = ingest_ext_event(&c, &ev, Some("device-test-1")).unwrap();
        assert_eq!(
            o2,
            IngestOutcome {
                written: 0,
                deduped: true
            }
        );

        // 不同 event_id：照常 append。复审 2026-11-11：幂等键在 event_data
        // JSON（0019 唯一索引），不再读 ExtEvent.dedup_key 字段——构造不同
        // 事件须改 payload 本体的 event_id。
        let mut ev2 = ev.clone();
        ev2.payload["event_id"] = serde_json::Value::from(format!("{}-x", TS_MS));
        let o3 = ingest_ext_event(&c, &ev2, None).unwrap();
        assert_eq!(o3.written, 1);

        let n: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM events WHERE event_type='window' \
                 AND event_action='tab_change' AND source_id='chrome_ext'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 2, "append-only：只增不删");

        let row: (String, String, String) = c
            .query_row(
                "SELECT timestamp, device_id, quality FROM events \
                 WHERE json_extract(event_data,'$.event_id') = ?1",
                rusqlite::params![format!("{TS_MS}-activated-42")],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(row.0, "2026-10-26T01:20:00+00:00");
        assert_eq!(row.1, "device-test-1");
        assert_eq!(row.2, "observed");
    }

    #[test]
    fn ingest_ai_session_row_matches_ai_adapter_shape() {
        let c = conn();
        let raw = format!(
            r#"{{"event_type":".ai_session","ts_ms":{ts},"event_data":{{"session_id":"chatgpt_web-tab9-{ts}","source":"chatgpt_web"}}}}"#,
            ts = TS_MS
        );
        let ev = parse_ext_message(raw.as_bytes()).unwrap();
        ingest_ext_event(&c, &ev, None).unwrap();
        ingest_ext_event(&c, &ev, None).unwrap(); // 幂等
        let n: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM events WHERE event_type='ai_session' \
                 AND event_action='ai_run' AND source_id='chrome_ext'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1);
        let sid: Option<String> = c
            .query_row(
                "SELECT json_extract(event_data,'$.event_data.session_id') FROM events \
                 WHERE event_type='ai_session' AND source_id='chrome_ext'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(sid.as_deref(), Some("chatgpt_web-tab9-1792977600000"));
    }

    #[test]
    fn device_id_reuses_core_metadata_mechanism() {
        let c = conn();
        let d1 = ensure_device(&c).unwrap();
        let d2 = ensure_device(&c).unwrap();
        assert_eq!(d1, d2, "同库重复取 device_id 必须稳定（metadata 表）");
        let stored: String = c
            .query_row(
                "SELECT value FROM metadata WHERE key='device_id'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(stored, d1);
    }

    #[test]
    fn manifest_carries_path_and_origins() {
        let m = build_host_manifest(
            r"C:\Program Files\kynoptic\kynoptic-browser-host.exe",
            &["aaaa_test_id"],
        );
        assert_eq!(m["name"], HOST_NAME);
        assert_eq!(m["type"], "stdio");
        assert_eq!(m["allowed_origins"][0], "chrome-extension://aaaa_test_id/");
        assert!(
            m["path"].as_str().unwrap().ends_with(".exe"),
            "Windows 上 path 必须是可执行文件绝对路径"
        );
        // 占位符必须被替换：生成器不接受未展开的占位 ID
        let m2 = build_host_manifest("x.exe", &["EXTENSION_ID_PLACEHOLDER"]);
        assert_eq!(
            m2["allowed_origins"][0],
            "chrome-extension://EXTENSION_ID_PLACEHOLDER/"
        );
    }
}
