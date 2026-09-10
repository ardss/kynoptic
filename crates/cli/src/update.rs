//! `kynoptic update`：从 GitHub Releases 自更新。
//!
//! 设计依据（v0.1 发行方式 = GitHub Releases 裸二进制）：
//! - self_update crate 检查最新 release，下载同名 bin 资产，Windows 上用
//!   rename-then-replace 规避"运行中 exe 不可覆盖"。
//! - 若用户是通过包管理器（winget/scoop/cargo）安装的，这里更新会破坏其
//!   包管理器状态——检测到此类路径特征时拒绝并提示改用对应工具。
//! - 仓库未发布或无网络时报清晰错误，不影响其他子命令。

/// 用户可能经由包管理器获得的安装路径特征：这些情况下拒绝自更新。
fn looks_like_package_manager_install(exe: &str) -> Option<&'static str> {
    let p = exe.to_ascii_lowercase();
    let panel = |s: &str| p.contains(s);
    if panel("\\scoop\\") {
        Some("scoop update kynoptic")
    } else if panel("\\winget\\") || panel("\\microsoft\\winget\\") {
        Some("winget upgrade kynoptic")
    } else if panel("\\.cargo\\bin\\") {
        Some("cargo install kynoptic")
    } else {
        None
    }
}

/// 当前 bin 名（kynoptic / kynoptic-ctl），更新对应资产。
fn update_bin_name() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.file_stem().map(|s| s.to_string_lossy().to_string()))
        .unwrap_or_else(|| "kynoptic".into())
}

fn e(x: self_update::errors::Error) -> crate::Error {
    crate::Error::InvalidData(format!("self-update: {x}"))
}

pub fn cmd_update(_args: &[String]) -> crate::Result<()> {
    let exe = std::env::current_exe()
        .map_err(|e| crate::Error::InvalidData(format!("无法定位当前可执行文件: {e}")))?;
    let exe = exe.to_string_lossy().to_string();
    if let Some(cmd) = looks_like_package_manager_install(&exe) {
        return Err(crate::Error::InvalidData(format!(
            "检测到本程序由包管理器安装（路径含其管理目录）。请改用: {cmd}"
        )));
    }
    // 安装版（Inno Setup）自更新会造成版本漂移：self_update 只换本 exe，
    // 托盘/看门狗仍是旧版，且卸载数据库记录与磁盘不一致（审查 P1）。
    // 有 unins000.exe 即认定安装版，指引重跑 Setup（新版发布后 Setup 可覆盖装）。
    if let Some(dir) = std::path::Path::new(&exe).parent() {
        if dir.join("unins000.exe").exists() {
            return Err(crate::Error::InvalidData(
                "检测到本程序为安装版。请重新下载并运行 Kynoptic-Setup 完成升级（自更新仅适用于便携版）".to_string(),
            ));
        }
    }

    let cur = self_update::cargo_crate_version!();
    eprintln!("checking GitHub releases for kynoptic v{cur}...");
    let status = self_update::backends::github::Update::configure()
        .repo_owner("ardss")
        .repo_name("kynoptic")
        .bin_name(&update_bin_name())
        .current_version(self_update::cargo_crate_version!())
        .no_confirm(true)
        .build()
        .map_err(e)?
        .update()
        .map_err(e)?;

    match status {
        self_update::Status::UpToDate(v) => println!("already up to date ({v})"),
        self_update::Status::Updated(v) => println!("updated to {v}"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::looks_like_package_manager_install as pm;

    #[test]
    fn detects_package_manager_paths() {
        assert_eq!(
            pm("C:\\Users\\x\\scoop\\apps\\kynoptic\\kynoptic.exe"),
            Some("scoop update kynoptic")
        );
        assert_eq!(
            pm("C:\\Users\\x\\.cargo\\bin\\kynoptic.exe"),
            Some("cargo install kynoptic")
        );
        assert_eq!(pm("C:\\tools\\kynoptic\\kynoptic.exe"), None);
    }
}
