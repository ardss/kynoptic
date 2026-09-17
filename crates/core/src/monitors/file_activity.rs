//! 文件活动监控
//!
//! 基于 ReadDirectoryChangesW 的目录树监控（原生 windows-sys）：
//! - 监控 Desktop / Documents / Downloads 三棵目录树（递归），
//!   过滤条件 FILE_NAME | DIR_NAME | LAST_WRITE | SIZE。
//! - 每个目录一个 watch 线程，原始变化经 channel 汇入 collect；
//!   collect 侧做 2 秒去抖聚合，一轮变化合并为一条 file_activity 事件。
//! - 生产节流：每分钟最多 20 条事件，超出的变化计入 truncated 计数，
//!   随下一条事件带回，防止删除整棵目录时的事件风暴。
//! - 错误路径：缓冲区溢出（ERROR_NOTIFY_ENUM_DIR 或 bytes==0）会丢弃
//!   本轮快照并立即重建 watch；句柄打开失败 5 秒后重试。

use crate::types::*;
use serde_json::json;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Storage::FileSystem::*;

/// 变化聚合去抖窗口
const DEBOUNCE: Duration = Duration::from_secs(2);
/// 每分钟最多产出的事件条数（防风暴）
const MAX_EVENTS_PER_MINUTE: u32 = 20;
/// 单条事件里最多明细条目
const MAX_DETAIL_ENTRIES: usize = 32;
/// pending 积压超过该数量立即冲刷
const MAX_PENDING: usize = 256;
/// watch 缓冲区大小
const BUFFER_SIZE: usize = 64 * 1024;

/// 一次原始文件变化
#[derive(Debug, Clone)]
pub struct RawChange {
    /// created / modified / renamed / deleted / overflow
    pub action: &'static str,
    /// 来源目录名（desktop / documents / downloads）
    pub root: String,
    /// 相对路径首段（文件或顶层目录名）
    pub seg: String,
}

struct State {
    started: bool,
    rx: Option<crossbeam_channel::Receiver<RawChange>>,
    /// (入队时间, 变化)
    pending: Vec<(Instant, RawChange)>,
    /// 节流窗口起点
    window_start: Option<Instant>,
    /// 本窗口已产事件数
    window_count: u32,
    /// 因节流被丢弃的变化条数（随下一条事件带回后清零）
    truncated: u64,
}

pub struct FileActivityMonitor {
    state: Mutex<State>,
}

impl Default for FileActivityMonitor {
    fn default() -> Self {
        Self {
            state: Mutex::new(State {
                started: false,
                rx: None,
                pending: Vec::new(),
                window_start: None,
                window_count: 0,
                truncated: 0,
            }),
        }
    }
}

impl Monitor for FileActivityMonitor {
    fn name(&self) -> &str {
        "file_activity"
    }

    fn interval(&self) -> Duration {
        Duration::from_secs(5)
    }

    fn collect(&self, tx: &crossbeam_channel::Sender<Event>) {
        let mut st = self.state.lock().unwrap();

        if !st.started {
            st.started = true;
            let (raw_tx, raw_rx) = crossbeam_channel::unbounded();
            st.rx = Some(raw_rx);
            for (root_name, path) in watch_roots() {
                spawn_watcher(&path, root_name, raw_tx.clone());
            }
        }

        // 吸干当前可用的原始变化
        let drained_raw: Vec<RawChange> = match st.rx.as_ref() {
            Some(rx) => {
                let mut v = Vec::new();
                while let Ok(c) = rx.try_recv() {
                    v.push(c);
                }
                v
            }
            None => Vec::new(),
        };
        for c in drained_raw {
            st.pending.push((Instant::now(), c));
        }

        // 去抖：最早的 pending 超过 2 秒（或积压过大）才合并产事件
        let ready = match st.pending.first() {
            Some((t, _)) => t.elapsed() >= DEBOUNCE,
            None => false,
        };
        if !ready && st.pending.len() < MAX_PENDING {
            return;
        }

        let drained: Vec<(Instant, RawChange)> = std::mem::take(&mut st.pending);
        if drained.is_empty() {
            return;
        }

        let total = drained.len();
        let mut details: Vec<serde_json::Value> = Vec::new();
        let mut roots: Vec<String> = Vec::new();
        for (_, c) in drained.iter().take(MAX_DETAIL_ENTRIES) {
            if !roots.contains(&c.root) {
                roots.push(c.root.clone());
            }
            if c.action == "overflow" {
                details.push(json!({ "action": "overflow", "root": c.root }));
            } else {
                details.push(json!({ "action": c.action, "root": c.root, "path": c.seg }));
            }
        }

        // 节流：每分钟最多 MAX_EVENTS_PER_MINUTE 条
        let now = Instant::now();
        let within_window = matches!(
            st.window_start,
            Some(start) if now.duration_since(start) < Duration::from_secs(60)
        );
        if !within_window {
            st.window_start = Some(now);
            st.window_count = 0;
        }
        if st.window_count >= MAX_EVENTS_PER_MINUTE {
            st.truncated += total as u64;
            return;
        }
        st.window_count += 1;

        let truncated = st.truncated;
        st.truncated = 0;

        let event = Event::new(EventAction::FileActivity, EventType::Device).data(json!({
            "changes": details,
            "change_count": total,
            "roots": roots,
            "truncated": truncated,
        }));
        let _ = tx.try_send(event);
    }
}

/// (目录名, 路径) 列表
fn watch_roots() -> Vec<(&'static str, String)> {
    vec![
        ("desktop", get_desktop_path()),
        ("documents", get_documents_path()),
        ("downloads", get_downloads_path()),
    ]
}

fn get_desktop_path() -> String {
    user_profile_dir("Desktop")
}
fn get_documents_path() -> String {
    user_profile_dir("Documents")
}
fn get_downloads_path() -> String {
    user_profile_dir("Downloads")
}
fn user_profile_dir(sub: &str) -> String {
    if let Ok(userprofile) = std::env::var("USERPROFILE") {
        format!("{}\\{}", userprofile, sub)
    } else {
        String::new()
    }
}

/// 为一个目录树启动 watch 线程。线程随进程生命周期运行；
/// watch 失败/溢出后自动重建。
pub fn spawn_watcher(path: &str, root_name: &str, tx: crossbeam_channel::Sender<RawChange>) {
    if path.is_empty() {
        return;
    }
    let path = path.to_string();
    let root_name = root_name.to_string();
    let stop = Arc::new(AtomicBool::new(false));
    std::thread::Builder::new()
        .name(format!("file-watch-{}", root_name))
        .spawn(move || watch_loop(&path, &root_name, &tx, &stop))
        .ok();
}

fn watch_loop(
    path: &str,
    root_name: &str,
    tx: &crossbeam_channel::Sender<RawChange>,
    stop: &AtomicBool,
) {
    loop {
        if stop.load(std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        let handle = open_watch_handle(path);
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            // 目录可能暂不可用（OneDrive 未就绪等），稍后重试
            std::thread::sleep(Duration::from_secs(5));
            continue;
        }

        let mut buffer = vec![0u8; BUFFER_SIZE];
        loop {
            if stop.load(std::sync::atomic::Ordering::Relaxed) {
                unsafe { CloseHandle(handle) };
                return;
            }
            let mut returned: u32 = 0;
            let filter = FILE_NOTIFY_CHANGE_FILE_NAME
                | FILE_NOTIFY_CHANGE_DIR_NAME
                | FILE_NOTIFY_CHANGE_LAST_WRITE
                | FILE_NOTIFY_CHANGE_SIZE;
            let ok = unsafe {
                ReadDirectoryChangesW(
                    handle,
                    buffer.as_mut_ptr() as *mut core::ffi::c_void,
                    BUFFER_SIZE as u32,
                    1, // 递归监控子树
                    filter,
                    &mut returned,
                    std::ptr::null_mut(),
                    None,
                )
            };

            if ok == 0 {
                let err = unsafe { GetLastError() };
                if err == ERROR_NOTIFY_ENUM_DIR {
                    // 缓冲区溢出：快照不完整，通知上层后重建 watch
                    let _ = tx.send(RawChange {
                        action: "overflow",
                        root: root_name.to_string(),
                        seg: String::new(),
                    });
                }
                break;
            }
            if returned == 0 {
                // 部分系统上溢出表现为成功但 0 字节
                let _ = tx.send(RawChange {
                    action: "overflow",
                    root: root_name.to_string(),
                    seg: String::new(),
                });
                break;
            }

            let changes = parse_notify_buffer(&buffer[..returned as usize]);
            let changes = merge_rename_pairs(changes);

            for (action, name) in changes {
                let Some(seg) = first_path_segment(&name) else {
                    continue;
                };
                let _ = tx.send(RawChange {
                    action: map_action(action),
                    root: root_name.to_string(),
                    seg,
                });
            }
        }

        unsafe {
            CloseHandle(handle);
        }
    }
}

fn open_watch_handle(path: &str) -> HANDLE {
    let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        CreateFileW(
            wide.as_ptr(),
            FILE_LIST_DIRECTORY,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            std::ptr::null_mut(),
        )
    }
}

/// 合并 rename 对：OLD_NAME + NEW_NAME 相邻出现，统一记为 renamed（用新名）。
/// 未配对的 OLD_NAME（重命名到监控树外）按删除（REMOVED）处理。
fn merge_rename_pairs(mut changes: Vec<(u32, String)>) -> Vec<(u32, String)> {
    let mut i = 0;
    while i < changes.len() {
        if changes[i].0 == FILE_ACTION_RENAMED_OLD_NAME {
            let is_pair = i + 1 < changes.len() && changes[i + 1].0 == FILE_ACTION_RENAMED_NEW_NAME;
            if is_pair {
                // 用 NEW_NAME 段的文件名替换被合并条目，再移除 NEW_NAME
                changes[i].0 = FILE_ACTION_RENAMED_NEW_NAME;
                changes[i].1 = std::mem::take(&mut changes[i + 1].1);
                changes.remove(i + 1);
            } else {
                changes[i].0 = FILE_ACTION_REMOVED;
            }
        }
        i += 1;
    }
    changes
}

/// 解析 FILE_NOTIFY_INFORMATION 链表，返回 (action, 文件名) 列表
pub fn parse_notify_buffer(buf: &[u8]) -> Vec<(u32, String)> {
    let mut out = Vec::new();
    let mut offset = 0usize;
    while offset + 12 <= buf.len() {
        let next = u32::from_le_bytes(buf[offset..offset + 4].try_into().unwrap()) as usize;
        let action = u32::from_le_bytes(buf[offset + 4..offset + 8].try_into().unwrap());
        let name_len =
            u32::from_le_bytes(buf[offset + 8..offset + 12].try_into().unwrap()) as usize;
        let name_start = offset + 12;
        if name_len == 0 || name_start + name_len > buf.len() {
            break;
        }
        let name_utf16: Vec<u16> = buf[name_start..name_start + name_len]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes(*c))
            .collect();
        out.push((action, String::from_utf16_lossy(&name_utf16)));
        if next == 0 {
            break;
        }
        offset += next;
    }
    out
}

/// 取相对路径首段（文件名或顶层子目录名）
fn first_path_segment(name: &str) -> Option<String> {
    // 去掉扩展路径前缀 \\?\
    let cleaned = name.strip_prefix("\\\\?\\").unwrap_or(name);
    cleaned
        .split('\\')
        .find(|s| !s.is_empty())
        .map(String::from)
}

fn map_action(action: u32) -> &'static str {
    match action {
        FILE_ACTION_ADDED => "created",
        FILE_ACTION_MODIFIED => "modified",
        FILE_ACTION_REMOVED => "deleted",
        FILE_ACTION_RENAMED_OLD_NAME | FILE_ACTION_RENAMED_NEW_NAME => "renamed",
        _ => "modified",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(std::path::PathBuf);
    impl TempDir {
        fn new(tag: &str) -> Self {
            let p = std::env::temp_dir().join(format!(
                "kynoptic-fa-test-{}-{}",
                tag,
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir_all(&p).unwrap();
            TempDir(p)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn recv_within(rx: &crossbeam_channel::Receiver<RawChange>, ms: u64) -> Vec<RawChange> {
        let deadline = Instant::now() + Duration::from_millis(ms);
        let mut out = Vec::new();
        while Instant::now() < deadline {
            if let Ok(c) = rx.recv_timeout(Duration::from_millis(100)) {
                out.push(c);
            }
        }
        out
    }

    #[test]
    fn parse_notify_buffer_minimal() {
        // 构造一条 FILE_NOTIFY_INFORMATION：action=1（ADDED），name="a.txt"
        let name: Vec<u16> = "a.txt".encode_utf16().collect();
        let mut buf = Vec::new();
        buf.extend_from_slice(&0u32.to_le_bytes()); // NextEntryOffset = 0
        buf.extend_from_slice(&1u32.to_le_bytes()); // Action
        buf.extend_from_slice(&((name.len() * 2) as u32).to_le_bytes());
        for c in name {
            buf.extend_from_slice(&c.to_le_bytes());
        }
        let parsed = parse_notify_buffer(&buf);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].0, FILE_ACTION_ADDED);
        assert_eq!(parsed[0].1, "a.txt");
    }

    #[test]
    fn parse_notify_buffer_two_entries_and_garbage() {
        let mk = |action: u32, name: &str, next: u32| {
            let mut b = Vec::new();
            b.extend_from_slice(&next.to_le_bytes());
            b.extend_from_slice(&action.to_le_bytes());
            let w: Vec<u16> = name.encode_utf16().collect();
            b.extend_from_slice(&((w.len() * 2) as u32).to_le_bytes());
            for c in w {
                b.extend_from_slice(&c.to_le_bytes());
            }
            b
        };
        // 第一条 22 字节（12 头 + 10 名字），NextEntryOffset 需 4 对齐取 24，
        // 因此真实布局里补 2 字节 padding
        let mut e1 = mk(FILE_ACTION_REMOVED, "x.bin", 24);
        e1.extend_from_slice(&[0u8, 0u8]);
        let e2 = mk(FILE_ACTION_ADDED, "y.bin", 0);
        let mut buf = e1;
        buf.extend_from_slice(&e2);
        let parsed = parse_notify_buffer(&buf);
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].0, FILE_ACTION_REMOVED);
        assert_eq!(parsed[1].1, "y.bin");

        // 垃圾数据不 panic
        assert!(parse_notify_buffer(&[0u8; 8]).is_empty());
        assert!(parse_notify_buffer(&[]).is_empty());
    }

    #[test]
    fn rename_pair_merges_to_new_name() {
        let merged = merge_rename_pairs(vec![
            (FILE_ACTION_RENAMED_OLD_NAME, "old.txt".into()),
            (FILE_ACTION_RENAMED_NEW_NAME, "new.txt".into()),
            (FILE_ACTION_MODIFIED, "keep.bin".into()),
        ]);
        assert_eq!(merged.len(), 2);
        // 配对成功：记为 renamed 且用新名，不保留旧名
        assert_eq!(merged[0].0, FILE_ACTION_RENAMED_NEW_NAME);
        assert_eq!(merged[0].1, "new.txt");
        assert_eq!(merged[1], (FILE_ACTION_MODIFIED, "keep.bin".to_string()));
    }

    #[test]
    fn unpaired_old_name_is_deleted() {
        // 重命名到监控树外：只剩 OLD_NAME，应按删除而非 renamed 处理
        let merged = merge_rename_pairs(vec![
            (FILE_ACTION_RENAMED_OLD_NAME, "gone.txt".into()),
            (FILE_ACTION_ADDED, "other.txt".into()),
        ]);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].0, FILE_ACTION_REMOVED);
        assert_eq!(merged[0].1, "gone.txt");
        assert_eq!(map_action(merged[0].0), "deleted");
    }

    #[test]
    fn first_path_segment_works() {
        assert_eq!(first_path_segment("a.txt"), Some("a.txt".into()));
        assert_eq!(first_path_segment("sub\\deep\\f.txt"), Some("sub".into()));
        assert_eq!(first_path_segment("\\\\?\\C:\\x"), Some("C:".into()));
        assert_eq!(first_path_segment(""), None);
    }

    #[test]
    fn temp_dir_real_round_trip() {
        let tmp = TempDir::new("roundtrip");
        let dir = tmp.0.to_string_lossy().to_string();
        let (tx, rx) = crossbeam_channel::unbounded();

        spawn_watcher(&dir, "testroot", tx);

        // 等待 watch 建立完成
        std::thread::sleep(Duration::from_millis(300));

        // 真实触发一轮：created → modified → renamed → deleted
        let f1 = tmp.0.join("alpha.txt");
        std::fs::write(&f1, b"hello").unwrap(); // created + modified
        std::fs::write(&f1, b"hello world").unwrap();

        let f2 = tmp.0.join("beta.txt");
        std::fs::rename(&f1, &f2).unwrap(); // rename pair
        std::fs::remove_file(&f2).unwrap(); // deleted

        let changes = recv_within(&rx, 4000);
        assert!(!changes.is_empty(), "watcher must produce raw changes");

        let actions: Vec<&str> = changes.iter().map(|c| c.action).collect();
        assert!(
            actions.contains(&"created"),
            "expected created in {:?}",
            actions
        );
        assert!(
            actions.contains(&"modified"),
            "expected modified in {:?}",
            actions
        );
        assert!(
            actions.contains(&"renamed"),
            "expected renamed in {:?}",
            actions
        );
        assert!(
            actions.contains(&"deleted"),
            "expected deleted in {:?}",
            actions
        );
        for c in &changes {
            assert_eq!(c.root, "testroot");
            assert!(!c.seg.is_empty());
        }
    }

    #[test]
    fn debounce_and_throttle_in_collect() {
        let monitor = FileActivityMonitor::default();
        let (tx, rx) = crossbeam_channel::unbounded();

        // 手工塞入 pending，绕过真实 watcher
        {
            let mut st = monitor.state.lock().unwrap();
            st.started = true;
            let (_t, r) = crossbeam_channel::unbounded();
            st.rx = Some(r);
            st.pending.push((
                Instant::now(),
                RawChange {
                    action: "created",
                    root: "downloads".into(),
                    seg: "a.iso".into(),
                },
            ));
        }

        // 去抖窗口内：不产事件
        monitor.collect(&tx);
        assert!(
            rx.try_recv().is_err(),
            "within debounce window must not emit"
        );

        // 把 pending 时间戳拨回 3 秒前：去抖通过，产事件
        {
            let mut st = monitor.state.lock().unwrap();
            st.pending[0].0 = Instant::now() - Duration::from_secs(3);
        }
        monitor.collect(&tx);
        let ev = rx
            .try_recv()
            .expect("after debounce window must emit file_activity");
        assert_eq!(ev.event_action.to_string(), "file_activity");
        let data = ev.event_data.unwrap();
        assert_eq!(data["change_count"], 1);
        assert_eq!(data["changes"][0]["action"], "created");
        assert_eq!(data["changes"][0]["path"], "a.iso");
        assert_eq!(data["truncated"], 0);

        // 节流：灌满每分钟配额后，多余的变化计入 truncated
        {
            let mut st = monitor.state.lock().unwrap();
            st.window_count = MAX_EVENTS_PER_MINUTE; // 配额耗尽
            st.pending.push((
                Instant::now() - Duration::from_secs(3),
                RawChange {
                    action: "deleted",
                    root: "desktop".into(),
                    seg: "b.txt".into(),
                },
            ));
        }
        monitor.collect(&tx);
        assert!(rx.try_recv().is_err(), "over quota must not emit");
        {
            let st = monitor.state.lock().unwrap();
            assert_eq!(st.truncated, 1);
        }

        // 下一轮配额恢复时，truncated 随事件带回并清零
        {
            let mut st = monitor.state.lock().unwrap();
            st.window_start = Some(Instant::now() - Duration::from_secs(61));
            st.pending.push((
                Instant::now() - Duration::from_secs(3),
                RawChange {
                    action: "modified",
                    root: "documents".into(),
                    seg: "c.docx".into(),
                },
            ));
        }
        monitor.collect(&tx);
        let ev = rx.try_recv().expect("new window must emit");
        let data = ev.event_data.unwrap();
        assert_eq!(data["truncated"], 1);
        {
            let st = monitor.state.lock().unwrap();
            assert_eq!(st.truncated, 0);
        }
    }
}
