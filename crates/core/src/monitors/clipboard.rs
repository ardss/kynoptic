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
            return Some(("non-text".into(), simple_hash(b"non-text"), 8));
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
        let max_u16 = (global_bytes / 2).max(1);
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

        Some(("text".into(), simple_hash(&bytes), byte_len))
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
fn simple_hash(data: &[u8]) -> [u8; 16] {
    let mut result = [0u8; 16];
    let mut state: [u64; 2] = [0xcbf29ce484222325, 0x100000001b3];
    for &byte in data {
        state[0] ^= byte as u64;
        state[0] = state[0].wrapping_mul(0x100000001b3);
        state[1] ^= byte as u64;
        state[1] = state[1].wrapping_mul(0x9E3779B97F4A7C15);
    }
    result[0..8].copy_from_slice(&state[0].to_le_bytes());
    result[8..16].copy_from_slice(&state[1].to_le_bytes());
    result
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
        let digest = simple_hash("hello".as_bytes());
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
        let a = simple_hash(b"hello");
        let a2 = simple_hash(b"hello");
        let b = simple_hash(b"hellp");
        assert_eq!(a, a2);
        assert_ne!(a, b);
        assert_eq!(hex8(&a).len(), 32);
    }

    #[test]
    fn hash_of_known_vector() {
        // 同一输入必须得到确定输出且非全零
        let d = simple_hash(b"");
        assert_eq!(d.len(), 16);
        assert_ne!(d, [0u8; 16]);
    }
}
