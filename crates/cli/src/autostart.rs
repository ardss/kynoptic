//! Windows 开机自启动：通过注册表 HKCU\Software\Microsoft\Windows\CurrentVersion\Run
//!
//! 仅当目标 cfg(target_os = "windows") 时此模块有实际意义；其他平台占位实现。
//! 错误类型统一为 [`kynoptic_core::Error`]（io 错误自动 `?` 转换）。

use kynoptic_core::Error;

#[cfg(target_os = "windows")]
mod imp {
    use super::Error;
    use std::path::Path;

    use winreg::enums::*;
    use winreg::RegKey;

    const RUN_KEY_PATH: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
    // Wave40 挂账：Run 值按安装目录指纹命名（与安装器同算法，见 core::naming）
    fn value_name() -> String {
        match kynoptic_core::naming::install_dir() {
            Some(d) => kynoptic_core::naming::run_value_name(&d),
            None => kynoptic_core::naming::LEGACY_RUN_VALUE_NAME.to_string(),
        }
    }

    /// 把 exe 路径 + 可选参数写入 Run 键。返回是否成功。
    pub fn enable(exe_path: &Path, args: &[&str]) -> Result<(), Error> {
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let (key, _disp) = hkcu.create_subkey(RUN_KEY_PATH)?;

        // exe 路径必须整体加引号（审查 P1：含空格路径会被 Run 键按前缀歧义解析）
        let mut cmd = format!("\"{}\"", exe_path.to_string_lossy());
        for a in args {
            if a.contains(' ') {
                cmd.push_str(&format!(" \"{}\"", a));
            } else {
                cmd.push(' ');
                cmd.push_str(a);
            }
        }
        // 用 set_value + &str 而不是 set_raw_value（winreg 0.52 API）
        key.set_value(value_name(), &cmd)?;
        // 升级清扫：旧版无指纹全局值名指向本 exe 时删掉，避免新旧双自启动
        if let Ok(legacy) = key.get_value::<String, _>(kynoptic_core::naming::LEGACY_RUN_VALUE_NAME)
        {
            if legacy.contains(&format!("\"{}\"", exe_path.display())) {
                let _ = key.delete_value(kynoptic_core::naming::LEGACY_RUN_VALUE_NAME);
            }
        }
        Ok(())
    }

    /// 删除 Run 键中的 Kynoptic 条目（无则视为成功）。旧版无指纹全局值名
    /// 指向本 exe 时一并删掉（免安装器升级场景：不清扫则关自启动后旧名
    /// 残留项继续生效；只清指向自己的，不动其他安装的值）。
    pub fn disable() -> Result<(), Error> {
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let key = hkcu.open_subkey_with_flags(RUN_KEY_PATH, KEY_ALL_ACCESS)?;
        let result = match key.delete_value(value_name()) {
            Ok(_) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(Error::Io(e)),
        };
        if result.is_ok() {
            if let Ok(exe) = std::env::current_exe() {
                let mine = format!("\"{}\"", exe.display());
                if let Ok(legacy) =
                    key.get_value::<String, _>(kynoptic_core::naming::LEGACY_RUN_VALUE_NAME)
                {
                    if legacy.contains(&mine) {
                        let _ = key.delete_value(kynoptic_core::naming::LEGACY_RUN_VALUE_NAME);
                    }
                }
            }
        }
        result
    }

    /// 查询当前是否启用（存在且值非空）。指纹名不存在时看旧版无指纹全局名
    ///（免安装器升级窗口：旧值指向本 exe 才算启用，不认其他安装的值）。
    pub fn is_enabled() -> Result<bool, Error> {
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        match hkcu.open_subkey(RUN_KEY_PATH) {
            Ok(key) => match key.get_value::<String, _>(&value_name()) {
                Ok(v) => Ok(!v.is_empty()),
                Err(_) => Ok(legacy_value_if_ours(&key).is_some()),
            },
            Err(_) => Ok(false),
        }
    }

    /// 查询当前值（指纹名不存在时回退旧全局名，仅限指向本 exe 的值）
    pub fn current() -> Result<Option<String>, Error> {
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let key = match hkcu.open_subkey(RUN_KEY_PATH) {
            Ok(k) => k,
            Err(_) => return Ok(None),
        };
        match key.get_value::<String, _>(&value_name()) {
            Ok(v) if !v.is_empty() => Ok(Some(v)),
            _ => Ok(legacy_value_if_ours(&key)),
        }
    }

    /// 旧版无指纹全局值名指向本 exe 时返回其值（升级窗口回退查询用）。
    fn legacy_value_if_ours(key: &RegKey) -> Option<String> {
        let legacy: String = key
            .get_value(kynoptic_core::naming::LEGACY_RUN_VALUE_NAME)
            .ok()?;
        if legacy.is_empty() {
            return None;
        }
        let exe = std::env::current_exe().ok()?;
        if legacy
            .to_lowercase()
            .contains(&exe.to_string_lossy().to_lowercase())
        {
            Some(legacy)
        } else {
            None
        }
    }
}

#[cfg(target_os = "windows")]
pub use imp::{current, disable, enable, is_enabled};

#[cfg(not(target_os = "windows"))]
mod imp {
    use super::Error;
    use std::path::Path;
    pub fn enable(_exe_path: &Path, _args: &[&str]) -> Result<(), Error> {
        Err(Error::InvalidData(
            "开机自启动仅在 Windows 上受支持".to_string(),
        ))
    }
    pub fn disable() -> Result<(), Error> {
        Err(Error::InvalidData(
            "开机自启动仅在 Windows 上受支持".to_string(),
        ))
    }
    pub fn is_enabled() -> Result<bool, Error> {
        Ok(false)
    }
    pub fn current() -> Result<Option<String>, Error> {
        Ok(None)
    }
}

#[cfg(not(target_os = "windows"))]
pub use imp::{current, disable, enable, is_enabled};
