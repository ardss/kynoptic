//! 托盘侧守护链共享文件路径(退出旗标 / 心跳)。
//!
//! **与 watchdog 的契约**:crates/cli/src/main.rs(watchdog 子命令)按同一套
//! 规则解析同名文件——文件名常量与解析优先级必须两边同步修改。
//! 统一规则:exe 同目录(两者都能从 current_exe() 稳定推到同一位置,不依赖
//! cwd 或 DB 路径;此前 tray 用 db.parent()、watchdog 用自己的 resolve_db_path(),
//! 两者不一致导致用户 Quit 后被反复拉起)。
//!
//! 优先级:
//! 1. 环境变量 `KYNOPTIC_EXIT_FLAG`(仅旗标;测试覆盖用,心跳无此机制)
//! 2. `--flag PATH` 托盘参数(main.rs 里处理,最终同样落到 exit_flag 变量)
//! 3. exe 同目录

use std::path::{Path, PathBuf};

/// "用户主动退出"旗标文件名(watchdog 据此区分 Quit 与被杀/崩溃)
pub const EXIT_FLAG_FILE: &str = "tray-exit.flag";
/// 采集心跳文件名(内容为 RFC3339 时间戳,每 30s touch 一次)
pub const HEARTBEAT_FILE: &str = "kynoptic-heartbeat";
/// 旗标路径覆盖环境变量(测试/多实例调试用)
pub const EXIT_FLAG_ENV: &str = "KYNOPTIC_EXIT_FLAG";

/// 单实例互斥体名（多用户/多会话隔离收口后 = `Global\KynopticTrayMutex\<SID>`）。
///
/// **契约（Wave29 挂账收口）**：tray / CLI collect / watchdog 探活三处一律经
/// `kynoptic_core::singleton::singleton_mutex_name()` 取名，禁止写死字面值——
/// 历史上三处各持同名常量靠测试锁字面值防漂移，现收敛为 core 单一事实源。
/// SID 获取失败时助手回退旧名 `Local\KynopticTrayMutex` 并留痕。
///
/// 兼容名常量仅供回退断言使用。
#[allow(dead_code)]
pub const SINGLE_INSTANCE_MUTEX_NAME: &str = kynoptic_core::singleton::LEGACY_MUTEX_NAME;

/// 当前进程应使用的单实例互斥体名（含用户 SID）。
pub fn single_instance_mutex_name() -> String {
    kynoptic_core::singleton::singleton_mutex_name()
}

/// 给定 exe 路径,推出旗标文件位置(纯函数,单测覆盖)。
pub fn exit_flag_for_exe(exe: &Path) -> Option<PathBuf> {
    exe.parent().map(|d| d.join(EXIT_FLAG_FILE))
}

/// 给定 exe 路径,推出心跳文件位置(纯函数,单测覆盖)。
pub fn heartbeat_for_exe(exe: &Path) -> Option<PathBuf> {
    exe.parent().map(|d| d.join(HEARTBEAT_FILE))
}

/// 解析退出旗标路径:env 覆盖 > exe 同目录 > cwd 兜底。
pub fn resolve_exit_flag() -> PathBuf {
    if let Ok(p) = std::env::var(EXIT_FLAG_ENV) {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    std::env::current_exe()
        .ok()
        .and_then(|e| exit_flag_for_exe(&e))
        .unwrap_or_else(|| PathBuf::from(EXIT_FLAG_FILE))
}

/// 解析心跳路径:exe 同目录 > cwd 兜底。
pub fn resolve_heartbeat() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|e| heartbeat_for_exe(&e))
        .unwrap_or_else(|| PathBuf::from(HEARTBEAT_FILE))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_instance_mutex_name_is_the_shared_contract() {
        // Wave29 挂账收口：名字改由 core 单一事实源派生
        //（Global\KynopticTrayMutex\<SID>），三处（tray/collect/watchdog）
        // 都调 kynoptic_core::singleton::singleton_mutex_name()，不再锁字面值；
        // 这里只锁定"派生名与助手一致 + 回退旧名保留"。
        assert_eq!(
            single_instance_mutex_name(),
            kynoptic_core::singleton::singleton_mutex_name()
        );
        assert_eq!(
            SINGLE_INSTANCE_MUTEX_NAME,
            kynoptic_core::singleton::LEGACY_MUTEX_NAME
        );
    }

    #[test]
    fn paths_anchor_to_exe_dir() {
        let exe = Path::new(r"C:\apps\kynoptic\kynoptic-tray.exe");
        assert_eq!(
            exit_flag_for_exe(exe).unwrap(),
            PathBuf::from(r"C:\apps\kynoptic\tray-exit.flag")
        );
        assert_eq!(
            heartbeat_for_exe(exe).unwrap(),
            PathBuf::from(r"C:\apps\kynoptic\kynoptic-heartbeat")
        );
    }

    #[test]
    fn exe_without_parent_falls_back_to_cwd_name() {
        // 裸文件名（无目录）: parent() 返回 Some(""), join 后是相对路径——
        // 语义上等于落在 cwd,与 resolve_* 的 cwd 兜底一致
        let p = exit_flag_for_exe(Path::new("kynoptic-tray.exe")).unwrap();
        assert_eq!(p, Path::new(EXIT_FLAG_FILE));
        let p = heartbeat_for_exe(Path::new("kynoptic-tray.exe")).unwrap();
        assert_eq!(p, Path::new(HEARTBEAT_FILE));
        // resolve_* 的兜底名与契约一致(watchdog 侧同名常量)
        assert_eq!(
            Path::new(EXIT_FLAG_FILE).file_name().unwrap(),
            "tray-exit.flag"
        );
        assert_eq!(
            Path::new(HEARTBEAT_FILE).file_name().unwrap(),
            "kynoptic-heartbeat"
        );
    }

    // env 是进程全局的,测试并行跑会互踩:两个 env 用例共用同一把锁串行化
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn env_override_wins_for_exit_flag() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::set_var(EXIT_FLAG_ENV, r"C:\tmp\test-flag");
        let p = resolve_exit_flag();
        std::env::remove_var(EXIT_FLAG_ENV);
        assert_eq!(p, PathBuf::from(r"C:\tmp\test-flag"));
    }

    #[test]
    fn empty_env_override_ignored() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::set_var(EXIT_FLAG_ENV, "");
        let p = resolve_exit_flag();
        std::env::remove_var(EXIT_FLAG_ENV);
        // 空 env 视为未设置,应落到 exe 同目录(含 tray-exit.flag 文件名)
        assert_eq!(p.file_name().unwrap(), "tray-exit.flag");
    }
}
