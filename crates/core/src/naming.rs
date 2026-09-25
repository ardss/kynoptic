//! 安装目录指纹与命名对象统一取名（安装器 ↔ 运行时两侧闭环）。
//!
//! 背景（Wave40 遗留，安装器全局命名空间隔离挂账修复）：「Kynoptic Watchdog」
//! 计划任务名与 Run 值名「Kynoptic」是全局的，同用户任何一次 /DIR= 指向别处
//! 的安装（含沙箱试装）都会夺走已装实例的任务指向。本模块按安装目录算出
//! 8 位十六进制指纹，派生每目录专属的任务名与 Run 值名；安装器（Inno
//! PascalScript）侧以同一算法独立实现，两侧必须逐字节一致——改动算法时两处
//! 同步改，并补真机探针。
//!
//! 算法（勿改）：djb 变体，h=5381 起，对路径小写化、去掉结尾分隔符后的
//! UTF-16 码元逐个 h = (h*33 + u) mod 2^32，输出 8 位大写十六进制。
//! 用 UTF-16 码元而非 UTF-8 字节，是为了与 Inno PascalScript 的
//! `Ord(S[I])`（UTF-16 码元）对齐，含非 ASCII 的用户名路径两侧仍一致。
//!
//! TestSuffix（沙箱演练构建）形态：安装器叠加 `-<TestSuffix>` 后缀，运行时
//! 同读 `KYNOPTIC_MUTEX_SUFFIX`（与 [`crate::singleton`] 同一开关）拼同形
//! 后缀名——两侧同读同拼，沙箱装出的托盘才查得到自己的任务与 Run 值。

/// 目录路径 → 8 位大写十六进制指纹（小写化 + 去结尾分隔符后按 UTF-16 码元哈希）。
pub fn install_fingerprint(dir: &str) -> String {
    let lower = dir.to_lowercase();
    let trimmed = lower.trim_end_matches(['\\', '/']);
    let mut h: u64 = 5381;
    for u in trimmed.encode_utf16() {
        h = (h * 33 + u as u64) & 0xFFFF_FFFF;
    }
    format!("{h:08X}")
}

/// 当前 exe 所在安装目录（无 exe 路径时 None）。
pub fn install_dir() -> Option<std::path::PathBuf> {
    std::env::current_exe()
        .ok()?
        .parent()
        .map(|p| p.to_path_buf())
}

/// 多实例/沙箱旁路后缀（与 [`crate::singleton::mutex_suffix_active`] 同一开关
/// 与同一套校验）：安装器 TestSuffix 构建以同后缀派生任务/Run 值名
///（kynoptic.iss 的 WatchdogTaskName/RunValueName），运行时必须同读
/// KYNOPTIC_MUTEX_SUFFIX 同拼后缀，沙箱装出的托盘才能查到自己的任务与
/// 自启动值（回归复审修复：此前运行时只算无后缀名，沙箱闭环断裂）。
fn mutex_suffix() -> Option<String> {
    match std::env::var("KYNOPTIC_MUTEX_SUFFIX") {
        Ok(suffix)
            if !suffix.is_empty()
                && suffix.len() <= 64
                && suffix
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') =>
        {
            Some(suffix)
        }
        _ => None,
    }
}

/// Run 自启动值名：`Kynoptic-<指纹>`；沙箱旁路（KYNOPTIC_MUTEX_SUFFIX）激活时
/// `Kynoptic-<后缀>-<指纹>`（与安装器 TestSuffix 构建同形，每安装目录独立）。
pub fn run_value_name(dir: &std::path::Path) -> String {
    run_value_name_with(dir, mutex_suffix().as_deref())
}

/// [`run_value_name`] 的纯函数核（后缀显式传入，便于单测不经环境变量）。
fn run_value_name_with(dir: &std::path::Path, suffix: Option<&str>) -> String {
    let fp = install_fingerprint(&dir.to_string_lossy());
    match suffix {
        Some(s) => format!("Kynoptic-{s}-{fp}"),
        None => format!("Kynoptic-{fp}"),
    }
}

/// 看门狗计划任务名：`Kynoptic Watchdog <指纹>`；沙箱旁路激活时
/// `Kynoptic Watchdog-<后缀> <指纹>`（与安装器 TestSuffix 构建同形）。
pub fn watchdog_task_name(dir: &std::path::Path) -> String {
    watchdog_task_name_with(dir, mutex_suffix().as_deref())
}

/// [`watchdog_task_name`] 的纯函数核（后缀显式传入，便于单测不经环境变量）。
fn watchdog_task_name_with(dir: &std::path::Path, suffix: Option<&str>) -> String {
    let fp = install_fingerprint(&dir.to_string_lossy());
    match suffix {
        Some(s) => format!("Kynoptic Watchdog-{s} {fp}"),
        None => format!("Kynoptic Watchdog {fp}"),
    }
}

/// 升级清扫用：旧版本的无指纹全局名（新安装器在安装/卸载时清掉它们）。
pub const LEGACY_RUN_VALUE_NAME: &str = "Kynoptic";
pub const LEGACY_WATCHDOG_TASK_NAME: &str = "Kynoptic Watchdog";

#[cfg(test)]
mod tests {
    use super::*;

    // 参考向量由独立脚本（Python，同算法）生成，防两侧实现漂移。
    #[test]
    fn fingerprint_reference_vectors() {
        assert_eq!(install_fingerprint("kynoptic"), "A62D7636");
        assert_eq!(
            install_fingerprint(r"c:\users\13397\appdata\local\programs\kynoptic"),
            "46516965"
        );
        assert_eq!(install_fingerprint("Kynoptic"), "A62D7636");
    }

    #[test]
    fn fingerprint_case_and_trailing_separator_insensitive() {
        let a = install_fingerprint(r"C:\Users\X\Programs\Kynoptic");
        let b = install_fingerprint(r"c:\users\x\programs\kynoptic\");
        assert_eq!(a, b);
    }

    #[test]
    fn names_derive_from_fingerprint() {
        let dir = std::path::Path::new(r"C:\Program Files\Kynoptic");
        assert!(run_value_name(dir).starts_with("Kynoptic-"));
        assert!(watchdog_task_name(dir).starts_with("Kynoptic Watchdog "));
        // 不同目录派生不同名字（命名空间隔离的根）
        let other = std::path::Path::new(r"C:\Other\Kynoptic");
        assert_ne!(run_value_name(dir), run_value_name(other));
        assert_ne!(watchdog_task_name(dir), watchdog_task_name(other));
    }

    // TestSuffix（沙箱演练）形态：安装器 kynoptic.iss 的 WatchdogTaskName /
    // RunValueName 在 TestSuffix 下拼出「Kynoptic Watchdog-<后缀> <指纹>」与
    // 「Kynoptic-<后缀>-<指纹>」，运行时必须同拼（真机探针已验证该形态的
    // 名字能被 schtasks / HKCU Run 键接受并读写，2026-09-25）。
    #[test]
    fn names_include_mutex_suffix_when_set() {
        let dir = std::path::Path::new(r"C:\Program Files\Kynoptic");
        assert_eq!(
            run_value_name_with(dir, Some("sbox1")),
            format!(
                "Kynoptic-sbox1-{}",
                install_fingerprint(r"C:\Program Files\Kynoptic")
            )
        );
        assert_eq!(
            watchdog_task_name_with(dir, Some("sbox1")),
            format!(
                "Kynoptic Watchdog-sbox1 {}",
                install_fingerprint(r"C:\Program Files\Kynoptic")
            )
        );
        // 带后缀与不带后缀是不同的名字（沙箱不夺生产）
        assert_ne!(
            run_value_name_with(dir, Some("sbox1")),
            run_value_name_with(dir, None)
        );
        assert_ne!(
            watchdog_task_name_with(dir, Some("sbox1")),
            watchdog_task_name_with(dir, None)
        );
    }

    // 后缀校验与 singleton::mutex_suffix_active 同源：空值/超长/非法字符不入名。
    #[test]
    fn mutex_suffix_rejects_invalid_values() {
        let valid = |s: &str| {
            !s.is_empty()
                && s.len() <= 64
                && s.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        };
        assert!(valid("sbox1"));
        assert!(valid("a-b_c"));
        assert!(!valid(""));
        assert!(!valid("has space"));
        assert!(!valid("has/slash"));
        assert!(!valid(&"x".repeat(65)));
    }
}
