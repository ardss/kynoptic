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
    const VALUE_NAME: &str = "Kynoptic";

    /// 把 exe 路径 + 可选参数写入 Run 键。返回是否成功。
    pub fn enable(exe_path: &Path, args: &[&str]) -> Result<(), Error> {
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let (key, _disp) = hkcu.create_subkey(RUN_KEY_PATH)?;

        let mut cmd = exe_path.to_string_lossy().to_string();
        for a in args {
            if a.contains(' ') {
                cmd.push_str(&format!(" \"{}\"", a));
            } else {
                cmd.push(' ');
                cmd.push_str(a);
            }
        }
        // 用 set_value + &str 而不是 set_raw_value（winreg 0.52 API）
        key.set_value(VALUE_NAME, &cmd)?;
        Ok(())
    }

    /// 删除 Run 键中的 Kynoptic 条目（无则视为成功）。
    pub fn disable() -> Result<(), Error> {
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let key = hkcu.open_subkey_with_flags(RUN_KEY_PATH, KEY_ALL_ACCESS)?;
        match key.delete_value(VALUE_NAME) {
            Ok(_) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(Error::Io(e)),
        }
    }

    /// 查询当前是否启用（存在且值非空）
    pub fn is_enabled() -> Result<bool, Error> {
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        match hkcu.open_subkey(RUN_KEY_PATH) {
            Ok(key) => match key.get_value::<String, _>(VALUE_NAME) {
                Ok(v) => Ok(!v.is_empty()),
                Err(_) => Ok(false),
            },
            Err(_) => Ok(false),
        }
    }

    /// 查询当前值
    pub fn current() -> Result<Option<String>, Error> {
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let key = match hkcu.open_subkey(RUN_KEY_PATH) {
            Ok(k) => k,
            Err(_) => return Ok(None),
        };
        match key.get_value::<String, _>(VALUE_NAME) {
            Ok(v) if !v.is_empty() => Ok(Some(v)),
            _ => Ok(None),
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
