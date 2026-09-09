//! 剪贴板变化监控
//!
//! 通过 Win32 剪贴板 API 检测内容变化，
//! 只记录内容类型的哈希摘要，不记录实际内容。

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
        let (content_type, digest) = read_clipboard_hash();

        let prev = self.last_digest.take();
        if prev.is_some() && prev == Some(digest) {
            self.last_digest.set(Some(digest));
            return;
        }
        self.last_digest.set(Some(digest));

        let event = Event::new(EventAction::Change, EventType::Clipboard).data(json!({
            "content_type": content_type,
        }));
        let _ = tx.try_send(event);
    }
}

/// 读取剪贴板内容类型和哈希摘要
fn read_clipboard_hash() -> (String, [u8; 16]) {
    unsafe {
        let cf_unicode_text: u32 = 13;

        if OpenClipboard(0) == 0 {
            return ("empty".into(), [0u8; 16]);
        }

        if IsClipboardFormatAvailable(cf_unicode_text) == 0 {
            CloseClipboard();
            return ("non-text".into(), simple_hash(b"non-text"));
        }

        let handle = GetClipboardData(cf_unicode_text);
        if handle.is_null() {
            CloseClipboard();
            return ("empty".into(), [0u8; 16]);
        }

        let ptr = GlobalLock(handle);
        if ptr.is_null() {
            CloseClipboard();
            return ("empty".into(), [0u8; 16]);
        }

        // 计算长度
        let mut len = 0usize;
        let mut p = ptr as *const u16;
        while *p != 0 {
            len += 1;
            p = p.add(1);
        }

        let bytes: Vec<u8> = std::slice::from_raw_parts(ptr as *const u8, len * 2).to_vec();
        GlobalUnlock(handle);
        CloseClipboard();

        ("text".into(), simple_hash(&bytes))
    }
}

extern "system" {
    fn OpenClipboard(hWndNewOwner: isize) -> i32;
    fn CloseClipboard() -> i32;
    fn IsClipboardFormatAvailable(format: u32) -> i32;
    fn GetClipboardData(format: u32) -> *mut std::ffi::c_void;
    fn GlobalLock(hMem: *mut std::ffi::c_void) -> *mut std::ffi::c_void;
    fn GlobalUnlock(hMem: *mut std::ffi::c_void) -> i32;
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
