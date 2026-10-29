//! 安装形态识别：安装版（Inno Setup）与便携版探测。
//!
//! 探测谓词的单一来源——cli 自更新拒绝闸（update.rs）与 tray 一键更新分流
//! （tray.rs 菜单标签 / UpdateNow）共用，禁止各处复制探测逻辑。
//!
//! **约束：Inno 卸载器编号随重装递增，须通配**——unins000.exe 只是首次安装的
//! 文件名，覆盖装/重装后为 unins001.exe、unins002.exe…（生产机实测仅存在
//! unins001.exe/unins001.dat）。逐字面名认定会漏判安装版 → 自更新拒绝闸
//! 失效、托盘一键更新不走下载页 → 卸载数据库与磁盘版本漂移。探测须按
//! unins*.exe 通配；仅 unins*.dat（无配套 exe）不构成安装版证据；无
//! uninstaller 目录判便携（便携行为不变）。

use std::path::Path;

/// 该目录是否为 Inno Setup 安装目录（存在 unins*.exe 卸载器）。
pub fn is_inno_installed_dir(dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false; // 目录不存在/不可读 → 判便携
    };
    entries
        .flatten()
        .any(|e| is_unins_exe_name(&e.file_name().to_string_lossy()))
}

/// 文件名是否形如 unins\\d+\\.exe（大小写不敏感）：Inno 卸载器编号至少
/// 一位数字；仅 unins*.dat（无配套 exe）不匹配。
fn is_unins_exe_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    let Some(rest) = lower.strip_prefix("unins") else {
        return false;
    };
    let Some(stem) = rest.strip_suffix(".exe") else {
        return false;
    };
    !stem.is_empty() && stem.bytes().all(|b| b.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    // === 安装版识别 unins 通配 ===
    // 临时目录用 pid + 进程内原子计数命名（时钟无关、并发安全），Drop 时
    // best-effort 清理。
    mod unins_wildcard {
        use super::*;
        use std::sync::atomic::{AtomicU32, Ordering};

        struct TempDir(std::path::PathBuf);

        impl TempDir {
            fn new(tag: &str) -> Self {
                static SEQ: AtomicU32 = AtomicU32::new(0);
                let n = SEQ.fetch_add(1, Ordering::Relaxed);
                let p = std::env::temp_dir().join(format!(
                    "kynoptic-unins-test-{}-{n}-{tag}",
                    std::process::id()
                ));
                std::fs::create_dir_all(&p).unwrap();
                TempDir(p)
            }

            fn path(&self) -> &Path {
                &self.0
            }

            fn touch(&self, name: &str) {
                std::fs::write(self.0.join(name), b"").unwrap();
            }
        }

        impl Drop for TempDir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }

        /// 首装 unins000 与递增编号 unins001/unins012 都须判安装版；
        /// 字面只认 unins000.exe 即漏判（闸失效 → 版本漂移）。
        #[test]
        fn detects_unins000_and_incremented_numbers() {
            let first = TempDir::new("first");
            first.touch("unins000.exe");
            assert!(
                is_inno_installed_dir(first.path()),
                "unins000.exe 须判安装版"
            );

            let reinstalled = TempDir::new("reinstalled");
            reinstalled.touch("unins001.exe");
            reinstalled.touch("unins001.dat");
            assert!(
                is_inno_installed_dir(reinstalled.path()),
                "Inno 重装后编号递增（unins001.exe）须判安装版，字面名单漏判即闸失效"
            );

            let much = TempDir::new("much");
            much.touch("unins012.exe");
            assert!(
                is_inno_installed_dir(much.path()),
                "unins012.exe（多位编号）须判安装版"
            );
        }

        /// 仅 unins*.dat（无配套 exe）、空目录、不存在目录均判便携
        /// （便携版行为不变）。
        #[test]
        fn dat_only_or_missing_dir_stays_portable() {
            let dat_only = TempDir::new("dat");
            dat_only.touch("unins001.dat");
            assert!(
                !is_inno_installed_dir(dat_only.path()),
                "仅 unins*.dat 无配套 exe 不构成安装版证据"
            );

            let empty = TempDir::new("empty");
            assert!(
                !is_inno_installed_dir(empty.path()),
                "无 uninstaller 目录判便携"
            );

            let missing = std::env::temp_dir().join("kynoptic-unins-nonexistent");
            assert!(!is_inno_installed_dir(&missing), "不存在目录判便携");
        }
    }
}
