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

/// 单实例互斥体名（Local\ 前缀 = 当前登录会话内唯一）。
///
/// **与 `kynoptic collect` 的契约**：crates/cli/src/main.rs 的 cmd_collect 用
/// 同名 CreateMutexW 做单实例保护——两个写者（tray 采集器 / CLI collect）绝
/// 不能并发写同一 SQLite 库（实测双写导致事件翻倍 + 双全局钩子）。tray 是
/// bin crate 无法被 cli 依赖，两边各持一份同名常量，各自用测试锁定字面值，
/// 改名必须两边同步。
pub const SINGLE_INSTANCE_MUTEX_NAME: &str = "Local\\KynopticTrayMutex";

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
        // 与 crates/cli/src/main.rs 的 SINGLE_INSTANCE_MUTEX_NAME 契约:
        // 同名字面值,任一侧改名必须两边同步(见常量注释)。
        assert_eq!(SINGLE_INSTANCE_MUTEX_NAME, r"Local\KynopticTrayMutex");
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
