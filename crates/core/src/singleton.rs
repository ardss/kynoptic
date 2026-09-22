//! 单实例互斥体名（多用户/多会话隔离收口，Wave29 遗留挂账）。
//!
//! 设计：安装为 per-user（%LOCALAPPDATA%\Programs），数据已按用户隔离；
//! 互斥体从 `Local\KynopticTrayMutex` 升级为 `Global\KynopticTrayMutex\<SID>`：
//! - `Global\` 前缀跨会话可见——同一用户在两个会话（如快速用户切换/SSH 进程
//!   内拉起）里启动也会互斥；
//! - 后缀当前用户 SID（TokenUser 查询）——不同用户互不干扰，各自有独立的
//!   单实例与 watchdog 探活。
//!
//! **契约**：tray（crates/tray/src/main.rs）、CLI collect（crates/cli/src/main.rs
//! 的 acquire_single_instance）与 watchdog（OpenMutexW 探活）三处都必须经
//! [`singleton_mutex_name`] 取名，禁止再写死字面值（历史上三处各自持同名
//! 常量，本模块成立后统一为单一事实源）。
//!
//! 机器级共享安装不受支持（文档口径）：全局名单用户隔离，两个不同用户同时
//! 跑是受支持的形态，不存在机器级单实例。

/// 旧名（SID 获取失败时的回退；保留旧语义 = 当前会话内互斥）
pub const LEGACY_MUTEX_NAME: &str = r"Local\KynopticTrayMutex";

/// 单实例互斥体名：`Global\KynopticTrayMutex\<当前用户 SID>`。
/// SID 不可用时回退旧名并 log::warn（降级留痕）。
pub fn singleton_mutex_name() -> String {
    match current_user_sid() {
        Some(sid) => format!("Global\\KynopticTrayMutex\\{sid}"),
        None => {
            log::warn!("无法获取当前用户 SID，单实例互斥体回退为会话级旧名 {LEGACY_MUTEX_NAME:?}");
            LEGACY_MUTEX_NAME.to_string()
        }
    }
}

/// 升级过渡桥：新版进程在持有新名之外，还应同时持有/探测旧名
/// `Local\KynopticTrayMutex`。旧版 tray/collect 只持旧名、只探旧名——若新版
/// 只看新名，升级窗口里新旧进程互相不可见，单实例保护（防同库双写、防
/// 双钩子）整体失效。双向兼容口径：
/// - 新持旧（本函数返回 Some(旧名) 时创建并持有）：旧版进程能看到新版实例；
/// - 新探旧：新版进程也能发现尚在运行的旧版实例。
///
/// 主名已是旧名（SID 回退）时返回 None——同名无需第二个句柄。
pub fn legacy_bridge_mutex_name(primary: &str) -> Option<&'static str> {
    (primary != LEGACY_MUTEX_NAME).then_some(LEGACY_MUTEX_NAME)
}

/// 查当前进程令牌的用户 SID 字符串（S-1-… 形态）。
///
/// 实现取 TokenUser + 手工格式化 SID 结构（不走 ConvertSidToStringSidW，
/// 免去 LocalFree 与额外 feature），全部落在 core 已启用的
/// Win32_System_Threading / Win32_Security features 内。
pub fn current_user_sid() -> Option<String> {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::Security::{GetTokenInformation, TokenUser, SID, TOKEN_QUERY};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    unsafe {
        let mut token: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return None;
        }
        // 第一次调用取所需缓冲区长度（必然 ERROR_INSUFFICIENT_BUFFER）
        let mut needed: u32 = 0;
        let _ = GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut needed);
        if needed == 0 {
            let _ = CloseHandle(token);
            return None;
        }
        let mut buf = vec![0u8; needed as usize];
        let ok = GetTokenInformation(
            token,
            TokenUser,
            buf.as_mut_ptr() as *mut core::ffi::c_void,
            needed,
            &mut needed,
        );
        let _ = CloseHandle(token);
        if ok == 0 {
            return None;
        }
        let token_user = &*(buf.as_ptr() as *const windows_sys::Win32::Security::TOKEN_USER);
        let sid = token_user.User.Sid as *const SID;
        if sid.is_null() {
            return None;
        }
        Some(format_sid(&*sid))
    }
}

/// 把 SID 结构格式化为 S-1-<authority>-<sub>… 字符串（纯函数，单测覆盖）。
fn format_sid(sid: &windows_sys::Win32::Security::SID) -> String {
    // 标识符授权是 48 位大端整数
    let authority: u64 = sid
        .IdentifierAuthority
        .Value
        .iter()
        .fold(0u64, |acc, &b| (acc << 8) | b as u64);
    let mut out = format!("S-1-{}", authority);
    // SubAuthorityCount 个 u32 小端子授权（结构里只内联声明 1 个，其余
    // 紧随其后——按 C 数组越界读取是 Win32 SID 的既定布局）
    let subs = unsafe {
        std::slice::from_raw_parts(sid.SubAuthority.as_ptr(), sid.SubAuthorityCount as usize)
    };
    for s in subs {
        out.push_str(&format!("-{}", s));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::Security::{SID, SID_IDENTIFIER_AUTHORITY};

    #[test]
    fn format_sid_well_known_shapes() {
        // S-1-1-0（Everyone）布局
        let everyone = SID {
            Revision: 1,
            SubAuthorityCount: 1,
            IdentifierAuthority: SID_IDENTIFIER_AUTHORITY_1,
            SubAuthority: [0],
        };
        assert_eq!(format_sid(&everyone), "S-1-1-0");
        // 典型用户 SID：S-1-5-21-<domain>-<domain>-<domain>-<rid>（NT 授权
        // 值 5，5 个子授权）。结构体只内联 1 个子授权，其余 4 个按 C 布局用
        // unsafe 构造（与 format_sid 的越界读取约定一致）。
        let user: SID = unsafe { std::mem::zeroed() };
        let mut user = user;
        user.Revision = 1;
        user.SubAuthorityCount = 5;
        user.IdentifierAuthority = SID_IDENTIFIER_AUTHORITY_5;
        user.SubAuthority[0] = 21;
        let subs = user.SubAuthority.as_ptr() as *mut u32;
        unsafe {
            *subs.add(1) = 1000;
            *subs.add(2) = 2000;
            *subs.add(3) = 3000;
            *subs.add(4) = 1001;
        }
        assert_eq!(format_sid(&user), "S-1-5-21-1000-2000-3000-1001");
    }

    // 标识符授权（6 字节大端）：值 1 = Everyone 类，值 5 = NT 授权
    const SID_IDENTIFIER_AUTHORITY_1: SID_IDENTIFIER_AUTHORITY = SID_IDENTIFIER_AUTHORITY {
        Value: [0, 0, 0, 0, 0, 1],
    };
    const SID_IDENTIFIER_AUTHORITY_5: SID_IDENTIFIER_AUTHORITY = SID_IDENTIFIER_AUTHORITY {
        Value: [0, 0, 0, 0, 0, 5],
    };

    #[test]
    fn current_user_sid_is_s1_shape() {
        // 真实进程令牌：本测试进程以当前用户运行，SID 必为 S-1-… 形态
        let sid = current_user_sid().expect("当前用户 SID 必须可取");
        assert!(sid.starts_with("S-1-"), "got {sid:?}");
        assert!(sid.split('-').count() >= 3);
    }

    #[test]
    fn singleton_mutex_name_is_global_and_per_user() {
        let name = singleton_mutex_name();
        if current_user_sid().is_some() {
            assert!(
                name.starts_with(r"Global\KynopticTrayMutex\S-1-"),
                "got {name:?}"
            );
            assert_eq!(
                name.rsplit('\\').next(),
                current_user_sid().as_deref(),
                "后缀必须是当前用户 SID"
            );
        } else {
            // 回退路径：保留旧名（此分支通常不触发，仅在令牌不可读的
            // 沙箱里生效）
            assert_eq!(name, LEGACY_MUTEX_NAME);
        }
    }
}
