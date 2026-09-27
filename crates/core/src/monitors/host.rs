//! 窗口属主「宿主感知」解析：宿主/封装 exe → 真实应用名
//!
//! 背景（发现 platform high）：前台窗口属主 pid 可能是宿主 exe 而非真实应用：
//! - UWP/内置 XAML 应用由 ApplicationFrameHost（及 TextInputHost/SearchHost
//!   等 SystemApps 宿主）承载窗口框架，真实应用跑在独立进程里（实测探针
//!   probe_uwp1f：ApplicationFrameHost 顶层窗口的子窗 0x209FA 属主 pid 20280
//!   = SystemSettings.exe——「设置」的真实应用进程）。
//! - 经典控制台窗属主恒为 conhost，cmd/powershell 只残留在窗口标题。
//!
//! 解析机制（全部纯 Win32 只读，无子进程；关键路径均经真机探针验证）：
//! 1. UWP/XAML 宿主：枚举宿主窗口的直接子窗，子窗属主 pid（≠ 宿主 pid 且
//!    非系统进程）即真实应用进程 → 取其 exe 名（探针实测 AFH→SystemSettings）。
//! 2. SystemApps/WindowsApps 宿主（TextInputHost 等）：宿主 exe 完整路径内嵌
//!    包家族名（实测 C:\Windows\SystemApps\MicrosoftWindows.Client.CBS_
//!    cw5n1h2txyewy\TextInputHost.exe）→ 取家族名（去 hash 后缀）作应用标识。
//! 3. conhost：窗口标题（= 控制台标题）token 与运行中进程基名精确匹配
//!    （如 "C:\Windows\system32\cmd.exe" → cmd.exe；取最右侧命中）。
//!    匹配不上则回退宿主名（展示层按共享宿主名单归一为友好标签，见下）。
//!
//! 探针铁律留痕：GetProcessWindowOwnershipInfo 在本机构建 shell32 无该
//! 入口点（探针实测 EntryPointNotFound），注册表 "Package Families"/
//! "Package Cache" 键亦不存在（探针实测）——故不依赖包管理器/注册表路径，
//! 只用上述已实测机制。
//!
//! 共享名单：[`HOST_EXES`] 与展示层白名单（dash/display.rs HOST_LABELS）
//! 是同一份「平台侧宿主名单」的两侧——本表是采集侧事实源，展示侧需保持
//! 同键（applicationframehost/textinputhost/conhost 三键已在展示侧，
//! searchhost/searchui 为采集侧新增，展示侧后续对齐）。

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

/// 宿主/封装窗口属主进程基名（小写、不含 .exe）。
/// 命中即触发宿主感知解析；未命中按普通应用处理。
pub const HOST_EXES: &[&str] = &[
    "applicationframehost",
    "textinputhost",
    "searchhost",
    "searchui",
    "conhost",
];

/// VM/RDP/远控宿主进程基名（小写、不含 .exe）：这些窗口属主的真实活动
/// 发生在客户机/远端，宿主机上不可观测（结构性漏报，只能客户机内再部署
/// 采集）。命中时在事件 data 打 remote_session 标注，配合「远程/虚拟机」
/// 分类展示。
pub const REMOTE_HOST_EXES: &[&str] = &[
    "mstsc",
    "xfreerdp",
    "mremote",
    "vmware-vmx",
    "vboxheadless",
    "todesk",
    "rustdesk",
    "anydesk",
    "sunloginclient",
    "vncviewer",
];

/// 子窗属主 pid 命中这些基名时不作为「真实应用」（宿主/系统 UI 组件）。
/// 与 process.rs 的 SKIP 名单同源口径（系统进程不进应用口径）。
const SYSTEM_CHILD_EXES: &[&str] = &[
    "dwm",
    "explorer",
    "csrss",
    "services",
    "lsass",
    "svchost",
    "fontdrvhost",
    "sihost",
    "taskhostw",
    "runtimebroker",
    "wininit",
    "smss",
    "system",
    "conhost",
    "applicationframehost",
    "textinputhost",
];

/// 进程名 → 归一化基名（小写、去 .exe）。宿主/远控名单匹配统一入口。
pub fn base_of(name: &str) -> String {
    let no_ext = name
        .trim()
        .strip_suffix(".exe")
        .or_else(|| name.trim().strip_suffix(".EXE"))
        .unwrap_or(name.trim());
    no_ext.to_ascii_lowercase()
}

/// 进程基名（小写、去 .exe）是否为宿主。
pub fn is_window_host(base: &str) -> bool {
    HOST_EXES.iter().any(|h| h.eq_ignore_ascii_case(base))
}

/// 进程基名（小写、去 .exe）是否为 VM/RDP/远控宿主。
pub fn is_remote_host(base: &str) -> bool {
    REMOTE_HOST_EXES
        .iter()
        .any(|h| base_of(base) == h.to_ascii_lowercase())
}

// ─── 宿主 → 真实应用解析 ─────────────────────────────────────────────────────

/// 宿主感知解析入口：属主基名（小写去 .exe）是宿主时尝试还原真实应用名。
///
/// 返回 `None` = 无法解析（调用方保留宿主 exe 名——展示层按共享宿主名单
/// 归一为友好标签，不冒充真实应用）：
/// - conhost：标题 token 未命中任何运行中进程；
/// - UWP 宿主：无「非宿主、非系统」属主的子窗，且路径无包家族名。
pub fn resolve_host_app(
    hwnd: HWND,
    host_pid: u32,
    host_base: &str,
    host_full_path: Option<&str>,
    title: &str,
) -> Option<String> {
    if host_base.eq_ignore_ascii_case("conhost") {
        return console_title_app(title);
    }
    // UWP/内置 XAML 宿主：子窗属主即真实应用进程（探针实测机制）
    if let Some(app) = child_window_app(hwnd, host_pid) {
        return Some(app);
    }
    // SystemApps/WindowsApps 宿主：exe 路径内嵌包家族名（探针实测）
    package_family_from_path(host_full_path)
}

/// conhost 窗口标题（= 控制台标题）→ 真实控制台应用。
///
/// 纯函数（进程快照由调用方从 [`process_bases`] 缓存取），单测覆盖：
/// - 标题按空白拆 token，路径 token 再取最后一段（反斜杠/斜杠）；
/// - token 小写去 .exe 后与「运行中进程基名」精确匹配；
/// - 多命中取最右侧（"cmd /k powershell" 语义：当前交互的是最后一个）。
fn console_title_app(title: &str) -> Option<String> {
    let running = process_bases();
    let mut best: Option<String> = None; // 命中基名（循环正序，后命中覆盖 → 最右优先）
    for tok in title.split_whitespace() {
        let seg = tok.rsplit(['\\', '/']).next().unwrap_or(tok);
        let key = base_of(seg);
        if key.len() < 2 {
            continue;
        }
        // 精确命中运行中进程基名（token 形态可带 .exe 后缀）
        if running.contains_key(&key) {
            best = Some(key);
        }
    }
    best.map(|k| format!("{k}.exe"))
}

/// 运行中进程基名缓存（小写去 .exe 为键）。60s TTL：console 标题解析只在
/// 前台切到 conhost 窗时发生（低频），快照成本摊薄；进程集合按基名去重
/// （同名多实例无差别）。测试经 [`set_process_bases_cache`] 注入。
static PROC_BASES: OnceLock<Mutex<(Instant, HashMap<String, u32>)>> = OnceLock::new();

fn process_bases() -> HashMap<String, u32> {
    let cache = PROC_BASES.get_or_init(|| Mutex::new((Instant::now(), HashMap::new())));
    let mut g = cache.lock().unwrap_or_else(|e| e.into_inner());
    if g.0.elapsed() > Duration::from_secs(60) {
        g.0 = Instant::now();
        g.1 = th32_process_bases();
    }
    g.1.clone()
}

/// 测试注入点（不读真实时钟/不碰真机快照）。
#[cfg(test)]
fn set_process_bases_cache(map: HashMap<String, u32>) {
    let cache = PROC_BASES.get_or_init(|| Mutex::new((Instant::now(), HashMap::new())));
    *cache.lock().unwrap() = (Instant::now(), map);
}

/// TH32 进程快照 → 基名(小写去.exe) → 任一 pid 的映射。
fn th32_process_bases() -> HashMap<String, u32> {
    use std::mem::{size_of, zeroed};
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::System::Diagnostics::ToolHelp::*;

    let mut out: HashMap<String, u32> = HashMap::new();
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap == INVALID_HANDLE_VALUE {
            return out;
        }
        let mut entry: PROCESSENTRY32W = zeroed();
        entry.dwSize = size_of::<PROCESSENTRY32W>() as u32;
        if Process32FirstW(snap, &mut entry) != 0 {
            loop {
                let end = entry
                    .szExeFile
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(entry.szExeFile.len());
                let name = String::from_utf16_lossy(&entry.szExeFile[..end]);
                if !name.is_empty() {
                    out.entry(base_of(&name)).or_insert(entry.th32ProcessID);
                }
                if Process32NextW(snap, &mut entry) == 0 {
                    break;
                }
            }
        }
        windows_sys::Win32::Foundation::CloseHandle(snap);
    }
    out
}

/// 枚举宿主窗口直接子窗，返回「非宿主、非系统」子窗属主的 exe 基名。
/// 探针机制：UWP/内置 XAML 应用的内容窗以子窗形式挂在宿主框架窗上，
/// 子窗属主 pid 即真实应用（AFH 场景实测 = SystemSettings）。
fn child_window_app(host_hwnd: HWND, host_pid: u32) -> Option<String> {
    unsafe {
        // GW_HWNDFIRSTCHILD=8（windows-sys 0.59 的 GET_WINDOW_CMD 未导出该常量，
        // 值与 WinUser.h 一致）
        const GW_HWNDFIRSTCHILD: u32 = 8;
        let mut child = GetWindow(host_hwnd, GW_HWNDFIRSTCHILD);
        while !child.is_null() {
            let mut pid: u32 = 0;
            GetWindowThreadProcessId(child, &mut pid);
            if pid != 0 && pid != host_pid {
                // 属主名永不空（OpenProcess 失败走 TH32 兜底，再失败 <elevated>
                // 标记）——标记名同样可能是「子窗属主已退出」的诚实信号
                let name = super::browser::get_process_name(pid);
                let base = base_of(&name);
                if !is_window_host(&base) && !SYSTEM_CHILD_EXES.iter().any(|s| s == &base) {
                    return Some(name);
                }
            }
            child = GetWindow(child, GW_HWNDNEXT);
        }
        None
    }
}

/// 宿主 exe 完整路径 → 包家族名（SystemApps/WindowsApps 目录段，去 `_hash`）。
/// 探针实测：TextInputHost 路径
/// `C:\Windows\SystemApps\MicrosoftWindows.Client.CBS_cw5n1h2txyewy\TextInputHost.exe`
/// → 家族名 `MicrosoftWindows.Client.CBS`（稳定应用标识，非 System32 宿主假名）。
pub fn package_family_from_path(full_path: Option<&str>) -> Option<String> {
    let path = full_path?;
    let lower = path.to_ascii_lowercase();
    // 家族名 = SystemApps\ / WindowsApps\ 之后的第一个路径段
    let seg = if let Some(m) = lower.find("systemapps\\") {
        &path[m + "systemapps\\".len()..]
    } else {
        let m = lower.find("windowsapps\\")?;
        &path[m + "windowsapps\\".len()..]
    };
    let family = seg.split('\\').next()?.trim();
    if family.is_empty() {
        return None;
    }
    // 去 hash 后缀（_cw5n1h2txyewy / _8wekyb3d8bbwe：下划线 + 纯字母数字；
    // 家族名本身不含下划线，误伤面限于「下划线+全字母数字」尾段）
    let family = match family.rfind('_') {
        Some(i) if i > 0 && family[i + 1..].chars().all(|c| c.is_ascii_alphanumeric()) => {
            &family[..i]
        }
        _ => family,
    };
    Some(family.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bases(names: &[&str]) -> HashMap<String, u32> {
        names
            .iter()
            .enumerate()
            .map(|(i, n)| (base_of(n), i as u32 + 1))
            .collect()
    }

    #[test]
    fn host_list_members() {
        assert!(is_window_host("applicationframehost"));
        assert!(is_window_host("conhost"));
        assert!(!is_window_host("chrome"));
        assert!(is_remote_host("mstsc"));
        assert!(is_remote_host("vmware-vmx"));
        assert!(!is_remote_host("code"));
    }

    /// conhost 标题解析（单测合并：进程基名缓存为进程级全局，串行注入
    /// 避免并行测试互相覆盖）。
    #[test]
    fn console_title_resolution() {
        set_process_bases_cache(bases(&["cmd.exe", "powershell.exe"]));
        // 默认标题 = 全路径：取最后一段命中
        assert_eq!(
            console_title_app(r"C:\Windows\System32\cmd.exe"),
            Some("cmd.exe".into())
        );
        // 命令串：最右侧命中优先
        assert_eq!(
            console_title_app("cmd /k powershell"),
            Some("powershell.exe".into())
        );
        // 纯路径 powershell 标题
        assert_eq!(
            console_title_app(r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe"),
            Some("powershell.exe".into())
        );

        // 收窄快照：未命中 token → None（调用方保留宿主名）
        set_process_bases_cache(bases(&["cmd.exe"]));
        assert_eq!(console_title_app("B:/"), None);
        assert_eq!(console_title_app("my custom title"), None);
        // 有命中 token 但该进程未在运行 → None
        assert_eq!(console_title_app("git status"), None);
    }

    #[test]
    fn package_family_strips_hash_suffix() {
        assert_eq!(
            package_family_from_path(Some(
                r"C:\Windows\SystemApps\MicrosoftWindows.Client.CBS_cw5n1h2txyewy\TextInputHost.exe"
            )),
            Some("MicrosoftWindows.Client.CBS".into())
        );
        assert_eq!(
            package_family_from_path(Some(
                r"C:\Program Files\WindowsApps\Microsoft.MSPaint_8wekyb3d8bbwe\Microsoft.MSPaint.exe"
            )),
            Some("Microsoft.MSPaint".into())
        );
        // 非包路径 → None
        assert_eq!(
            package_family_from_path(Some(r"C:\Windows\System32\ApplicationFrameHost.exe")),
            None
        );
        assert_eq!(package_family_from_path(None), None);
    }

    #[test]
    fn base_of_normalizes() {
        // 纯进程名归一（base_of 不做路径切分——路径切分在 console_title_app
        // 的 rsplit 完成；此处只覆盖「大小写 + .exe 后缀 + 两侧空格」）
        assert_eq!(base_of("CMD.EXE"), "cmd");
        assert_eq!(base_of("chrome.exe"), "chrome");
        assert_eq!(base_of("chrome"), "chrome");
        assert_eq!(base_of("  ConHost.exe  "), "conhost");
    }
}
