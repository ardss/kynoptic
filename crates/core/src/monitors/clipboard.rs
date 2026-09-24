//! 剪贴板变化监控
//!
//! 通过 Win32 剪贴板 API 检测内容变化，
//! 只记录内容类型的哈希摘要，不记录实际内容。
//!
//! 审查 P2（幻影事件防线）：
//! - OpenClipboard 失败（被其他进程占用）不改变 last_digest，不产生事件；
//! - 首轮轮询只建基线，不发"变化"事件。

use crate::types::*;
use serde_json::json;
use std::cell::Cell;
use std::time::Duration;

pub struct ClipboardMonitor {
    last_digest: Cell<Option<[u8; 16]>>,
}

impl Default for ClipboardMonitor {
    fn default() -> Self {
        Self {
            last_digest: Cell::new(None),
        }
    }
}

impl Monitor for ClipboardMonitor {
    fn name(&self) -> &str {
        "clipboard"
    }
    fn interval(&self) -> Duration {
        Duration::from_secs(5)
    }

    fn collect(&self, tx: &crossbeam_channel::Sender<Event>) {
        let prev = self.last_digest.get();
        // 审查 P2：读不到剪贴板（OpenClipboard 被其他进程占用等）时保持上次
        // 状态原样返回——旔回退 ("empty", 全零 digest) 会造成 digest 翻转，
        // 产生幻影 Change 事件。
        let Some((content_type, digest, len)) = read_clipboard_hash() else {
            return;
        };
        // 首轮只建基线，不发事件（审查：第一轮 prev 为 None 时必然
        // "翻转"，无条件发一条"剪贴板变化"是幻影事件）
        if prev.is_none() {
            self.last_digest.set(Some(digest));
            return;
        }
        if prev == Some(digest) {
            return;
        }
        self.last_digest.set(Some(digest));

        // 只记元数据：digest 前 8 hex + 内容字节数，不写内容本身
        let digest_hex = hex8(&digest);
        let event = Event::new(EventAction::Change, EventType::Clipboard).data(json!({
            "content_type": content_type,
            "digest": &digest_hex[..8],
            "len": len,
        }));
        let _ = tx.try_send(event);
    }
}

/// 读取剪贴板内容类型、哈希摘要与字节长度。
/// 打不开剪贴板（OpenClipboard 失败）返回 None——调用方保持上次状态不变，
/// 不把它当成"内容清空"处理。
fn read_clipboard_hash() -> Option<(String, [u8; 16], usize)> {
    unsafe {
        let cf_unicode_text: u32 = 13;

        if OpenClipboard(0) == 0 {
            return None;
        }

        if IsClipboardFormatAvailable(cf_unicode_text) == 0 {
            CloseClipboard();
            return Some(("non-text".into(), simple_hash(&salt(), b"non-text"), 8));
        }

        let handle = GetClipboardData(cf_unicode_text);
        if handle.is_null() {
            CloseClipboard();
            return Some(("empty".into(), [0u8; 16], 0));
        }

        let ptr = GlobalLock(handle);
        if ptr.is_null() {
            CloseClipboard();
            return Some(("empty".into(), [0u8; 16], 0));
        }

        // 计算长度（Wave20 P2：以 GlobalSize 为上界——异常应用写入未 NUL
        // 终止的数据时纯信任剪贴板会越界读）
        let global_bytes = GlobalSize(handle);
        // 审查：GlobalSize < 2 时连一个 u16（2 字节）都装不下，按空内容
        // 处理——此前 `.max(1)` 会强读 1 个 u16，越界读到的字节混入
        // simple_hash 与 byte_len，污染去重哈希（正是本上界要防的场景）。
        if global_bytes < 2 {
            GlobalUnlock(handle);
            CloseClipboard();
            return Some(("empty".into(), [0u8; 16], 0));
        }
        let max_u16 = global_bytes / 2;
        let mut len = 0usize;
        let mut p = ptr as *const u16;
        while len < max_u16 && *p != 0 {
            len += 1;
            p = p.add(1);
        }

        let byte_len = len * 2;
        let bytes: Vec<u8> = std::slice::from_raw_parts(ptr as *const u8, byte_len).to_vec();
        GlobalUnlock(handle);
        CloseClipboard();

        Some(("text".into(), simple_hash(&salt(), &bytes), byte_len))
    }
}

extern "system" {
    fn OpenClipboard(hWndNewOwner: isize) -> i32;
    fn CloseClipboard() -> i32;
    fn IsClipboardFormatAvailable(format: u32) -> i32;
    fn GetClipboardData(format: u32) -> *mut std::ffi::c_void;
    fn GlobalLock(hMem: *mut std::ffi::c_void) -> *mut std::ffi::c_void;
    fn GlobalUnlock(hMem: *mut std::ffi::c_void) -> i32;
    fn GlobalSize(hMem: *mut std::ffi::c_void) -> usize;
}

/// 简单哈希用于去重（不需要密码学安全）
///
/// 审查修复：改为**加盐**哈希——盐首次创建库时随机生成、存 metadata 表
/// （`clipboard_salt`），同一明文在不同库得到不同 digest，拿到库文件的
/// 攻击者无法复用针对其他库预计算的字典/彩虹表离线还原短内容。
/// 注意：digest 仅防跨库预计算比对，对极短明文（如纯数字口令）携带盐的
/// 定向穷举仍然可行——文档需注明 digest 不是加密。
fn simple_hash(salt: &[u8; 16], data: &[u8]) -> [u8; 16] {
    let mut result = [0u8; 16];
    let mut state: [u64; 2] = [0xcbf29ce484222325, 0x100000001b3];
    // 盐作为前缀进入同一混淆循环：换盐即全量换 digest
    for &byte in salt.iter().chain(data.iter()) {
        state[0] ^= byte as u64;
        state[0] = state[0].wrapping_mul(0x100000001b3);
        state[1] ^= byte as u64;
        state[1] = state[1].wrapping_mul(0x9E3779B97F4A7C15);
    }
    result[0..8].copy_from_slice(&state[0].to_le_bytes());
    result[8..16].copy_from_slice(&state[1].to_le_bytes());
    result
}

use std::sync::OnceLock;

/// 进程内全局盐。单一存储：`salt()` 读、`init_salt_from_db`/`set_salt` 写。
/// 正式路径由 collector 启动时经 [`init_salt_from_db`] 从库 metadata
/// 加载/创建；未接线的调用方惰性生成进程内随机盐兜底。
static SALT: OnceLock<[u8; 16]> = OnceLock::new();

/// 当前盐（进程内全局；正式路径由 collector 启动时经 [`init_salt_from_db`]
/// 从库 metadata 加载/创建，未接线的调用方惰性生成进程内随机盐兜底）。
pub(crate) fn salt() -> [u8; 16] {
    *SALT.get_or_init(rand::random)
}

/// 从库 metadata 加载（或首次创建）剪贴板/通知摘要盐，并设为进程内全局盐。
/// 由 collector 启动时调用（此时 db 已打开）。
pub(crate) fn init_salt_from_db(db: &crate::db::Database) {
    let s: [u8; 16] = match db.get_metadata("clipboard_salt") {
        Some(hex) => decode_hex16(&hex).unwrap_or_else(|| {
            log::warn!("metadata clipboard_salt 非法（需 32 位 hex），重新生成");
            new_salt(db)
        }),
        None => new_salt(db),
    };
    // 写入模块级单一 SALT；若此前已被惰性兜底初始化（理论不可达：init
    // 先于任何采集），保持首次值。两个 OnceLock 已合并，init 的盐会被
    // salt() 读到。
    let _ = set_salt(s);
}

fn new_salt(db: &crate::db::Database) -> [u8; 16] {
    let s: [u8; 16] = rand::random();
    db.set_metadata("clipboard_salt", &encode_hex(&s));
    s
}

fn set_salt(s: [u8; 16]) -> Result<(), [u8; 16]> {
    SALT.set(s)
}

fn encode_hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn decode_hex16(hex: &str) -> Option<[u8; 16]> {
    if hex.len() != 32 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut out = [0u8; 16];
    for (i, chunk) in hex.as_bytes().chunks(2).enumerate() {
        let hi = (chunk[0] as char).to_digit(16)? as u8;
        let lo = (chunk[1] as char).to_digit(16)? as u8;
        out[i] = hi * 16 + lo;
    }
    Some(out)
}

/// 供 notification 监控复用：对内容取加盐摘要，返回完整 32 位 hex。
pub(crate) fn salted_digest_hex(data: &[u8]) -> String {
    hex8(&simple_hash(&salt(), data))
}

/// digest 转 hex（payload 取前 8 位）
fn hex8(digest: &[u8; 16]) -> String {
    digest.iter().map(|b| format!("{:02x}", b)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex8(digest: &[u8; 16]) -> String {
        digest.iter().map(|b| format!("{:02x}", b)).collect()
    }

    fn hex8_local(digest: &[u8; 16]) -> String {
        digest.iter().map(|b| format!("{:02x}", b)).collect()
    }

    #[test]
    fn payload_contains_digest_and_len() {
        let digest = simple_hash(&[0u8; 16], "hello".as_bytes());
        let len = "hello".len() * 2; // UTF-16 字节数

        // 复刻 collect 的 payload 组装逻辑（不依赖真实剪贴板状态）
        let digest_hex = hex8_local(&digest);
        let payload = json!({
            "content_type": "text",
            "digest": &digest_hex[..8],
            "len": len,
        });

        assert!(payload["digest"].is_string(), "payload must contain digest");
        assert_eq!(payload["digest"].as_str().unwrap().len(), 8);
        assert!(payload["digest"]
            .as_str()
            .unwrap()
            .chars()
            .all(|c| c.is_ascii_hexdigit()));
        assert_eq!(payload["len"].as_u64().unwrap(), 10);
        assert_eq!(payload["content_type"], "text");
        // 不写内容本身
        assert!(payload.get("content").is_none());
    }

    #[test]
    fn digest_is_stable_and_discriminating() {
        let s = [7u8; 16];
        let a = simple_hash(&s, b"hello");
        let a2 = simple_hash(&s, b"hello");
        let b = simple_hash(&s, b"hellp");
        assert_eq!(a, a2);
        assert_ne!(a, b);
        assert_eq!(hex8(&a).len(), 32);
    }

    #[test]
    fn salt_changes_digest_and_blocks_cross_salt_tables() {
        // 同一明文在不同盐下 digest 不同（跨库字典/预表失效）
        let a = simple_hash(&[1u8; 16], b"123456");
        let b = simple_hash(&[2u8; 16], b"123456");
        assert_ne!(a, b);
    }

    #[test]
    fn decode_hex16_roundtrip() {
        let s: [u8; 16] = std::array::from_fn(|i| i as u8);
        assert_eq!(decode_hex16(&encode_hex(&s)), Some(s));
        assert_eq!(decode_hex16("zz"), None);
        assert_eq!(decode_hex16("abcd"), None); // 长度不对
    }

    #[test]
    fn hash_of_known_vector() {
        // 同一输入必须得到确定输出且非全零
        let d = simple_hash(&[0u8; 16], b"");
        assert_eq!(d.len(), 16);
        assert_ne!(d, [0u8; 16]);
    }

    /// 接线回归测试：init_salt_from_db 写入的盐必须被 salt() 读到
    /// （曾因 salt()/set_salt() 各自声明同名 OnceLock 而断裂）。
    #[test]
    fn init_salt_from_db_feeds_salt() {
        let path = std::env::temp_dir().join(format!(
            "kyn-clipboard-salt-{}-{}.db",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let db = crate::db::Database::open(path.to_str().unwrap()).unwrap();
        // 首开无盐：生成并持久化
        init_salt_from_db(&db);
        let stored = db.get_metadata("clipboard_salt").expect("盐必须持久化");
        assert_eq!(stored.len(), 32);
        let persisted = decode_hex16(&stored).unwrap();
        if SALT.get().is_some() {
            // 本进程内 salt() 已被其它路径先初始化（OnceLock 不可重设）：
            // 只能断言持久化盐合法，不能断言相等。
            let _ = persisted;
        } else {
            // 未被抢先初始化：salt() 必须返回刚持久化的盐（接线成立）
            assert_eq!(salt(), persisted, "salt() 必须读到 init 写入的持久化盐");
        }
    }
}
