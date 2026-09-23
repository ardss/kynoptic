//! `kynoptic update`：从 GitHub Releases 自更新（便携版五件套）。
//!
//! 设计依据（v0.1 发行方式 = GitHub Releases 裸二进制）：
//! - 审查 P0：旧实现走 self_update 的 `{bin}-{version}-{target}.zip` 资产只换
//!   kynoptic.exe 一个文件，造成 tray/watchdog 版本漂移。现改为手动流程：
//!   下载 SHA256SUMS.txt + 五个裸 exe + SKILL.md 资产（release 同时上传 zip
//!   供手动下载），全部通过 SHA-256 校验后才替换；SKILL.md 在 exe 替换
//!   成功后刷新到 exe 同目录（文档文件，失败仅告警不回滚）。
//! - 审查 P0：替换前旧 exe 改名 `.bak` 保留；新 kynoptic.exe 启动失败时
//!   从 `.bak` 整体还原。校验失败则整体放弃、不动任何旧文件。
//! - 审查（结构修复）：替换两阶段化——全部旧文件先挪 `.bak`，再全部放入
//!   新文件；任一步失败统一从 `.bak` 还原（rollback 逐项检查结果，失败
//!   如实报「回滚未完成」），消除逐文件交替 rename 留下「exe 缺失、仅存
//!   .bak」的中断窗口；入口与 watchdog 拉起前做孤儿 `.bak` 恢复扫描。
//! - tray 运行中被锁定时：先写退出旗标再 taskkill 静默结束
//!   （CREATE_NO_WINDOW）解锁；仍失败则整体回滚并要求关闭进程后重跑。
//! - 若用户是通过包管理器（winget/scoop/cargo）安装的，这里更新会破坏其
//!   包管理器状态——检测到此类路径特征时拒绝并提示改用对应工具。
//! - SHA-256 为本文件内手写实现（受"仅允许改 update.rs"约束，不能在
//!   Cargo.toml 新增 sha2 依赖），带 FIPS 180-4 测试向量。

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

fn io_err(x: std::io::Error) -> crate::Error {
    crate::Error::InvalidData(format!("self-update io: {x}"))
}

/// 便携版自更新必须整体替换的五件套（与 release.yml 上传的 release assets
/// 一致——五个裸 exe 均作为独立资产上传，自更新逐个下载校验）。
/// 注意：[0] 必须是主 exe kynoptic.exe——替换顺序逻辑（先换 [1..] 再换 [0]）
/// 依赖这一约定，勿改。ctl/aggrepair 无常驻进程，沿用同一 rename 流程即可。
const BIN_NAMES: [&str; 5] = [
    "kynoptic.exe",
    "kynoptic-tray.exe",
    "kynoptic-watchdog.exe",
    "kynoptic-ctl.exe",
    "kynoptic-aggrepair.exe",
];
/// 随更新分发的 SKILL.md（release 资产名；成功替换后落到 exe 同目录）。
/// 校验/下载走 ASSET_NAMES（五件套 + SKILL.md），exe 替换仍只走 BIN_NAMES，
/// 避免把文档文件塞进 BIN_NAMES[1..]+[0] 的进程解锁/启动验证流程。
const SKILL_MD_NAME: &str = "SKILL.md";
/// 完整资产清单（完整性检查与下载用）：五件套 + SKILL.md。
const ASSET_NAMES: [&str; 6] = [
    "kynoptic.exe",
    "kynoptic-tray.exe",
    "kynoptic-watchdog.exe",
    "kynoptic-ctl.exe",
    "kynoptic-aggrepair.exe",
    SKILL_MD_NAME,
];
const SUMS_NAME: &str = "SHA256SUMS.txt";
const REPO_OWNER: &str = "ardss";
const REPO_NAME: &str = "kynoptic";

// ---------------------------------------------------------------------------
// SHA-256（FIPS 180-4），见文件头注释说明
// ---------------------------------------------------------------------------
mod sha256 {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];

    pub struct Sha256 {
        state: [u32; 8],
        buffer: [u8; 64],
        buffered: usize,
        length: u64,
    }

    impl Sha256 {
        pub fn new() -> Self {
            Self {
                state: [
                    0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c,
                    0x1f83d9ab, 0x5be0cd19,
                ],
                buffer: [0u8; 64],
                buffered: 0,
                length: 0,
            }
        }

        pub fn update(&mut self, mut data: &[u8]) {
            self.length = self.length.wrapping_add(data.len() as u64);
            if self.buffered > 0 {
                let take = std::cmp::min(64 - self.buffered, data.len());
                self.buffer[self.buffered..self.buffered + take].copy_from_slice(&data[..take]);
                self.buffered += take;
                data = &data[take..];
                if self.buffered == 64 {
                    let block = self.buffer;
                    self.compress(&block);
                    self.buffered = 0;
                }
            }
            while data.len() >= 64 {
                let (block, rest) = data.split_at(64);
                let mut b = [0u8; 64];
                b.copy_from_slice(block);
                self.compress(&b);
                data = rest;
            }
            if !data.is_empty() {
                self.buffer[..data.len()].copy_from_slice(data);
                self.buffered = data.len();
            }
        }

        pub fn finalize(mut self) -> [u8; 32] {
            let bits = self.length.wrapping_mul(8);
            self.update(&[0x80]);
            while self.buffered != 56 {
                self.update(&[0]);
            }
            // 上面 update 已把 length 也计入，改用手动补长度避免重复统计：
            // 此时 buffered == 56（或刚压缩后为 0 再补到 56），直接填 8 字节大端位长
            self.length = 0;
            let mut last = [0u8; 8];
            last.copy_from_slice(&bits.to_be_bytes());
            self.update(&last);
            let mut out = [0u8; 32];
            for (i, w) in self.state.iter().enumerate() {
                out[i * 4..i * 4 + 4].copy_from_slice(&w.to_be_bytes());
            }
            out
        }

        fn compress(&mut self, block: &[u8; 64]) {
            let mut w = [0u32; 64];
            for i in 0..16 {
                w[i] = u32::from_be_bytes([
                    block[i * 4],
                    block[i * 4 + 1],
                    block[i * 4 + 2],
                    block[i * 4 + 3],
                ]);
            }
            for i in 16..64 {
                let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
                let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
                w[i] = w[i - 16]
                    .wrapping_add(s0)
                    .wrapping_add(w[i - 7])
                    .wrapping_add(s1);
            }
            let mut h = self.state;
            for i in 0..64 {
                let ch = (h[4] & h[5]) ^ ((!h[4]) & h[6]);
                let maj = (h[0] & h[1]) ^ (h[0] & h[2]) ^ (h[1] & h[2]);
                let s0 = h[0].rotate_right(2) ^ h[0].rotate_right(13) ^ h[0].rotate_right(22);
                let s1 = h[4].rotate_right(6) ^ h[4].rotate_right(11) ^ h[4].rotate_right(25);
                let t1 = h[7]
                    .wrapping_add(s1)
                    .wrapping_add(ch)
                    .wrapping_add(K[i])
                    .wrapping_add(w[i]);
                let t2 = s0.wrapping_add(maj);
                h[7] = h[6];
                h[6] = h[5];
                h[5] = h[4];
                h[4] = h[3].wrapping_add(t1);
                h[3] = h[2];
                h[2] = h[1];
                h[1] = h[0];
                h[0] = t1.wrapping_add(t2);
            }
            for (s, &v) in self.state.iter_mut().zip(h.iter()) {
                *s = s.wrapping_add(v);
            }
        }
    }

    pub fn hex_digest(data: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(data);
        hasher
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }
}

// ---------------------------------------------------------------------------
// 辅助：版本比较 / SUMS 解析 / 下载 / 进程操作
// ---------------------------------------------------------------------------

/// 点分数字版本比较（语义化版本的保守近似；无法解析的部分按字符串比）
fn version_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    let pa: Vec<Option<u64>> = a.split('.').map(|s| s.parse::<u64>().ok()).collect();
    let pb: Vec<Option<u64>> = b.split('.').map(|s| s.parse::<u64>().ok()).collect();
    for i in 0..std::cmp::max(pa.len(), pb.len()) {
        let (x, y) = (pa.get(i).copied().flatten(), pb.get(i).copied().flatten());
        match (x, y) {
            (Some(x), Some(y)) => {
                if x != y {
                    return x.cmp(&y);
                }
            }
            (Some(_), None) => return std::cmp::Ordering::Greater,
            (None, Some(_)) => return std::cmp::Ordering::Less,
            (None, None) => {}
        }
    }
    std::cmp::Ordering::Equal
}

/// 解析 SHA256SUMS.txt（`<hex>  <name>` 行）→ 名称(小写) → hex(小写)
fn parse_sums(text: &str) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some((hash, name)) = line.split_once(char::is_whitespace) {
            let name = name.trim_start();
            map.insert(name.to_ascii_lowercase(), hash.to_ascii_lowercase());
        }
    }
    map
}

fn download_file(url: &str, dest: &std::path::Path) -> crate::Result<()> {
    use std::io::Write;
    let mut f = std::fs::File::create(dest).map_err(io_err)?;
    let name_err = |err| crate::Error::InvalidData(format!("self-update header name: {err}"));
    let value_err = |err| crate::Error::InvalidData(format!("self-update header value: {err}"));
    self_update::Download::from_url(url)
        .show_progress(true)
        .set_header(
            "Accept".parse().map_err(name_err)?,
            "application/octet-stream".parse().map_err(value_err)?,
        )
        .download_to(&mut f)
        .map_err(e)?;
    f.flush().map_err(io_err)?;
    Ok(())
}

/// 静默强杀进程（taskkill，CREATE_NO_WINDOW 不弹 console 窗）
#[cfg(windows)]
fn kill_process(image: &str) -> bool {
    // 自杀防护（审查 P2）：替换循环里包含主 exe（更新器自身）；rename 运行中
    // 的 exe 在 Windows 上通常成功，只有失败兜底才走到杀进程——此时按镜像名
    // 杀会把自己（及一切 kynoptic CLI 会话）中途击毙，托盘已换、回滚不再运行。
    if let Ok(self_exe) = std::env::current_exe() {
        if self_exe
            .file_name()
            .map(|n| n.to_string_lossy() == image)
            .unwrap_or(false)
        {
            return false;
        }
    }
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000; // windows-sys Win32::System::Threading::CREATE_NO_WINDOW
    std::process::Command::new("taskkill")
        .args(["/F", "/IM", image, "/T"])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[cfg(not(windows))]
fn kill_process(_image: &str) -> bool {
    false
}

/// 新 exe 是否可执行（隐藏窗口跑 `--version`，成功退出才算）
#[cfg(windows)]
fn verify_launch(exe: &std::path::Path) -> bool {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    std::process::Command::new(exe)
        .arg("--version")
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[cfg(not(windows))]
fn verify_launch(_exe: &std::path::Path) -> bool {
    true
}

/// 把下载好的新文件替换到 dest：旧文件改名 `.bak` 保留；被占用时对
/// tray/watchdog 先 taskkill 再试一次；仍失败返回 false（跳过）。
/// 托盘退出旗标路径（与 tray/paths.rs 同口径：exe 同目录 tray-exit.flag）。
fn exit_flag_for(exe_dir: &std::path::Path) -> std::path::PathBuf {
    exe_dir.join("tray-exit.flag")
}

/// 从 `.bak` 还原单个文件。每步检查结果：旧实现 `let _ =` 吞错，新 exe 被
/// 杀软以独占句柄锁定时 remove/rename 都会失败，磁盘终态为「新版在位、旧版
/// 只剩 .bak」，程序却报「已回滚」。返回 false 即还原未完成。
fn restore_from_bak(dest: &std::path::Path, bak: &std::path::Path) -> bool {
    let _ = std::fs::remove_file(dest);
    std::fs::rename(bak, dest).is_ok()
}

/// 更新失败时从 `.bak` 整体还原（审查 P0-2）。返回是否全部还原成功；
/// 任一失败都如实上报，绝不假报「已回滚」。
fn rollback(backups: &[(std::path::PathBuf, std::path::PathBuf)], reason: &str) -> bool {
    eprintln!("{reason}，回滚到旧版本...");
    let mut ok = true;
    for (dest, bak) in backups {
        if bak.exists() && !restore_from_bak(dest, bak) {
            ok = false;
            eprintln!(
                "回滚失败: {} 无法还原（可能被杀软/备份软件锁定），旧版保留于 {}",
                dest.display(),
                bak.display()
            );
        }
    }
    // Wave18 P1：回滚=更新失败退出，托盘已被杀且不会由本进程拉起——
    // 旗标残留会让看门狗永久不拉起托盘（等于托盘凭空消失到下次重启）。
    if let Some(dir) = backups.first().and_then(|(d, _)| d.parent()) {
        let _ = std::fs::remove_file(exit_flag_for(dir));
    }
    ok
}

/// 启动期孤儿 `.bak` 恢复扫描（审查：替换流程中断——断电/被 taskkill /T
/// 波及——会留下「exe 缺失、仅存 .bak」的目录，此前无人复原，watchdog 也
/// 只会报「缺失，无法拉起」）。五件套任一 dest 缺失而 dest.bak 在位即改名
/// 还原，返回还原个数。cmd_update 与 watchdog 拉起路径在入口调用。
pub fn recover_orphan_baks(exe_dir: &std::path::Path) -> usize {
    let mut n = 0;
    for name in BIN_NAMES {
        let dest = exe_dir.join(name);
        let bak = std::path::PathBuf::from(format!("{}.bak", dest.display()));
        if !dest.exists() && bak.exists() && std::fs::rename(&bak, &dest).is_ok() {
            eprintln!("已从 {} 还原缺失的 {}", bak.display(), dest.display());
            n += 1;
        }
    }
    n
}

// ---------------------------------------------------------------------------
// 主流程
// ---------------------------------------------------------------------------

/// `update --check`：只查不装。打印 `UPDATE <ver>`（有新版）或
/// `UP TO DATE`，退出码恒 0。供托盘每日自动检查复用（托盘无 HTTP 客户端，
/// 子进程 + 文件是唯一低成本通路）。安装版也允许查（只有安装才被拒）。
pub fn is_stable_release(version: &str) -> bool {
    // semver 预发布约定：tag 含 '-' 即非稳定（v0.2.0-1 这类带后缀 tag 同样
    // 被拒——文档化的有意决定，测试直接调用本函数防同义反复）
    if version.contains('-') {
        return false;
    }
    // 修复（审查 low）：仅靠 '-' 过滤会放过 v0.2.1x / v0.3.0beta 这类无连字符
    // 的非法 tag——version_cmp 对非数字尾缀组件解析为 None，比较结果不可靠
    //（v0.2.1x 恰为最新时会被误判 already up to date / 被提示更新到非法 tag）。
    // 要求 tag 严格匹配 ^v?\d+(\.\d+){0,2}$ 才参与比较与 release 选择。
    let v = version.strip_prefix('v').unwrap_or(version);
    let parts: Vec<&str> = v.split('.').collect();
    (1..=3).contains(&parts.len())
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
}

pub fn cmd_check_only() -> crate::Result<()> {
    let cur = self_update::cargo_crate_version!();
    match latest_stable_version().ok_or_else(|| {
        crate::Error::InvalidData("GitHub Releases 上找不到完整的稳定版资产".to_string())
    })? {
        Some(v) => {
            println!("UPDATE {v}");
        }
        None => {
            println!("UP TO DATE ({cur})");
        }
    }
    Ok(())
}

/// 查询最新稳定版版本号；无更新（<= 当前）返回 None。网络/资产错误返回
/// None 并打日志——检查失败必须静默降级，不能打扰用户。
pub fn latest_stable_version() -> Option<Option<String>> {
    let releases = self_update::backends::github::ReleaseList::configure()
        .repo_owner(REPO_OWNER)
        .repo_name(REPO_NAME)
        .build()
        .ok()?
        .fetch()
        .ok()?;
    let cur = self_update::cargo_crate_version!();
    let release = releases.into_iter().find(|r| {
        is_stable_release(&r.version)
            && BIN_NAMES
                .iter()
                .all(|n| r.assets.iter().any(|a| &a.name == n))
            && r.assets.iter().any(|a| a.name == SUMS_NAME)
    })?;
    let new_ver = release.version.trim_start_matches('v').to_string();
    if version_cmp(&new_ver, cur) == std::cmp::Ordering::Greater {
        Some(Some(new_ver))
    } else {
        Some(None)
    }
}

pub fn cmd_update(_args: &[String]) -> crate::Result<()> {
    // 自更新按 current_exe 文件名收敛到 kynoptic：更新会整体替换五件套，
    // 从副 exe 入口发起会与主入口并发替换同一批文件，故一律收敛到主 exe。
    if update_bin_name() != "kynoptic" {
        return Err(crate::Error::InvalidData(
            "请改用 kynoptic update 完成自更新（tray/watchdog/ctl/aggrepair 五件套随其一并替换）"
                .to_string(),
        ));
    }
    let exe = std::env::current_exe()
        .map_err(|err| crate::Error::InvalidData(format!("无法定位当前可执行文件: {err}")))?;
    let exe = exe.to_string_lossy().to_string();
    if let Some(cmd) = looks_like_package_manager_install(&exe) {
        return Err(crate::Error::InvalidData(format!(
            "检测到本程序由包管理器安装（路径含其管理目录）。请改用: {cmd}"
        )));
    }
    // 安装版（Inno Setup）自更新会造成版本漂移：卸载数据库记录与磁盘不一致。
    // 有 unins000.exe 即认定安装版，指引重跑 Setup（新版发布后 Setup 可覆盖装）。
    let exe_path = std::path::PathBuf::from(&exe);
    let dir = exe_path
        .parent()
        .ok_or_else(|| crate::Error::InvalidData("无法定位安装目录".to_string()))?
        .to_path_buf();
    if dir.join("unins000.exe").exists() {
        return Err(crate::Error::InvalidData(
            "检测到本程序为安装版。请重新下载并运行 Kynoptic-Setup 完成升级（自更新仅适用于便携版）".to_string(),
        ));
    }
    // 审查：上次更新中断可能留下「exe 缺失、仅存 .bak」——更新入口先做
    // 孤儿恢复，避免在残缺目录上继续替换。
    recover_orphan_baks(&dir);

    let cur = self_update::cargo_crate_version!();
    eprintln!("checking GitHub releases for kynoptic v{cur}...");

    // 1. 取最新 release（要求同时具备三件套 + SHA256SUMS 资产）
    let releases = self_update::backends::github::ReleaseList::configure()
        .repo_owner(REPO_OWNER)
        .repo_name(REPO_NAME)
        .build()
        .map_err(e)?
        .fetch()
        .map_err(e)?;
    let release = releases
        .into_iter()
        .find(|r| {
            // prerelease 过滤（审查 P1）：workflow 对任何 v* tag 都出全量资产，
            // 不过滤会让 0.1.x 稳定用户被"更到"beta/RC（version_cmp 对
            // 非数字尾缀组件的比较不可靠）。self_update 0.41 的 Release 不暴露
            // prerelease 标志，用 semver 预发布约定（tag 含 '-'）判定。
            is_stable_release(&r.version)
                && ASSET_NAMES
                    .iter()
                    .all(|n| r.assets.iter().any(|a| &a.name == n))
                && r.assets.iter().any(|a| a.name == SUMS_NAME)
        })
        .ok_or_else(|| {
            crate::Error::InvalidData(
                "GitHub Releases 上找不到完整的更新资产（五件套 + SKILL.md + SHA256SUMS.txt）"
                    .to_string(),
            )
        })?;
    let new_ver = release.version.trim_start_matches('v').to_string();
    if version_cmp(&new_ver, cur) != std::cmp::Ordering::Greater {
        println!("already up to date ({cur})");
        // Wave17：顺手清提示文件——一键更新成功后托盘进程仍是旧版，
        // 靠 update-available.txt 判断的菜单项会再挂最长 24h。
        let _ = std::fs::remove_file(dir.join("data").join("update-available.txt"));
        return Ok(());
    }
    eprintln!("downloading kynoptic v{new_ver}...");

    // 2. 下载全部资产到临时目录
    let tmp = self_update::TempDir::new()
        .map_err(|err| crate::Error::InvalidData(format!("self-update tempdir: {err}")))?;
    let asset_url = |name: &str| -> crate::Result<String> {
        release
            .assets
            .iter()
            .find(|a| a.name == name)
            .map(|a| a.download_url.clone())
            .ok_or_else(|| crate::Error::InvalidData(format!("缺少资产 {name}")))
    };
    let sums_path = tmp.path().join(SUMS_NAME);
    download_file(&asset_url(SUMS_NAME)?, &sums_path)?;
    let sums_text = std::fs::read_to_string(&sums_path).map_err(io_err)?;
    let sums = parse_sums(&sums_text);

    let mut downloaded: Vec<(String, std::path::PathBuf)> = Vec::new();
    // 下载走完整资产清单（含 SKILL.md）；SHA-256 校验循环遍历同一列表，
    // 因此 SKILL.md 的哈希必须出现在 SHA256SUMS.txt（release.yml 已覆盖全部
    // dist 文件），缺失或校验失败同样整体放弃。
    for name in ASSET_NAMES {
        let path = tmp.path().join(name);
        download_file(&asset_url(name)?, &path)?;
        downloaded.push((name.to_string(), path));
    }

    // 3. SHA-256 校验：任何一个失败/缺失都整体放弃，不动旧文件（审查 P0-2）
    for (name, path) in &downloaded {
        let expect = sums.get(&name.to_ascii_lowercase()).ok_or_else(|| {
            crate::Error::InvalidData(format!("SHA256SUMS.txt 缺少 {name} 的校验值，放弃更新"))
        })?;
        let data = std::fs::read(path).map_err(io_err)?;
        let actual = sha256::hex_digest(&data);
        if actual != *expect {
            return Err(crate::Error::InvalidData(format!(
                "{name} 校验失败（期望 {expect}，实际 {actual}），已放弃更新，旧文件未被改动"
            )));
        }
    }

    // 4. 替换（结构性重组，替换整体原子化）：旧流程逐文件「bak→放入」交替
    // 进行，窗口内断电/被杀会留下「exe 缺失、仅存 .bak」。现改为两阶段：
    // 4a 全部旧文件先挪 .bak（含 tray 解锁），4b 全部新文件放入；任一失败
    // 统一从 .bak 还原（rollback 逐项检查结果，失败如实上报）。顺序仍为
    // tray/watchdog 先、主 exe 最后，缩短主 exe 不在位窗口。
    let mut backups: Vec<(std::path::PathBuf, std::path::PathBuf)> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    let mut tray_killed = false;
    let mut order: Vec<&str> = BIN_NAMES[1..].to_vec();
    order.push(BIN_NAMES[0]);

    // 4a. 备份：旧文件改名 .bak（dest 原不存在时无 .bak，rollback 跳过）
    for name in &order {
        let dest = dir.join(name);
        let bak = std::path::PathBuf::from(format!("{}.bak", dest.display()));
        let _ = std::fs::remove_file(&bak);
        if dest.exists() && std::fs::rename(&dest, &bak).is_err() {
            // 沿用旧 replace_with_backup 的托盘解锁：先写退出旗标再 taskkill
            if name == &"kynoptic-tray.exe" {
                if let Some(d) = dest.parent() {
                    let _ = std::fs::write(exit_flag_for(d), "updating\n");
                }
                if kill_process(name) {
                    tray_killed = true;
                    std::thread::sleep(std::time::Duration::from_millis(500));
                }
                if std::fs::rename(&dest, &bak).is_ok() {
                    backups.push((dest, bak));
                    continue;
                }
                // Wave17 审查 P1：旗标残留会永久压制看门狗拉起——失败路径必须清
                if let Some(d) = dest.parent() {
                    let _ = std::fs::remove_file(exit_flag_for(d));
                }
            }
            // 契约同 4b/步骤 5：回滚成败如实上报，绝不假报"已回滚"
            let rolled = rollback(&backups, &format!("{name} 被占用无法备份"));
            let detail = if rolled {
                "已回滚到旧版本".to_string()
            } else {
                "回滚未完成：旧版保留于同名 .bak，请关闭占用进程后手动还原".to_string()
            };
            return Err(crate::Error::InvalidData(format!(
                "部分文件被占用未能替换（可能被杀软/备份软件锁定），{detail}；请关闭所有 kynoptic 进程后重跑 update"
            )));
        }
        backups.push((dest, bak));
    }

    // 4b. 放入新文件；任一失败统一从 .bak 还原
    for name in &order {
        let (_, src) = downloaded
            .iter()
            .find(|(n, _)| n == name)
            .ok_or_else(|| crate::Error::InvalidData(format!("内部错误：缺少 {name}")))?;
        let dest = dir.join(name);
        if std::fs::rename(src, &dest).is_err() {
            if name == &"kynoptic-tray.exe" && tray_killed {
                warnings.push("托盘未更新，请退出托盘后重跑 update".to_string());
            } else {
                warnings.push(format!("{name} 被占用未更新，请关闭后重跑 update"));
            }
            let rolled = rollback(&backups, "新文件放入失败");
            let detail = if rolled {
                "已回滚到旧版本".to_string()
            } else {
                "回滚未完成：旧版保留于同名 .bak，请关闭占用进程后手动还原".to_string()
            };
            let mut msg = format!(
                "部分文件被占用未能替换（可能被杀软/备份软件锁定），{detail}；请关闭所有 kynoptic 进程后重跑 update"
            );
            for w in &warnings {
                msg.push_str(&format!("；{w}"));
            }
            return Err(crate::Error::InvalidData(msg));
        }
    }

    // 5. 新主 exe 启动验证，失败整体回滚（审查 P0-2）
    if !verify_launch(&dir.join("kynoptic.exe")) {
        if rollback(&backups, "新 kynoptic.exe 启动验证失败") {
            return Err(crate::Error::InvalidData(
                "新版本启动失败，已回滚到旧版本".to_string(),
            ));
        }
        return Err(crate::Error::InvalidData(
            "新版本启动失败，且回滚未完成：当前 exe 为新版本，旧版保留于同名 .bak，请手动还原后反馈问题"
                .to_string(),
        ));
    }

    // 6. SKILL.md 刷新到 exe 目录（已过 SHA-256 校验）。文档文件不做 .bak/
    // 回滚——失败仅告警，不影响更新结果。
    if let Some((_, src)) = downloaded.iter().find(|(n, _)| n.as_str() == SKILL_MD_NAME) {
        match std::fs::copy(src, dir.join(SKILL_MD_NAME)) {
            Ok(_) => println!("SKILL.md updated"),
            Err(err) => warnings.push(format!(
                "SKILL.md 刷新失败（{err}），可稍后用 skill install 重装"
            )),
        }
    }

    println!("updated to {new_ver}");
    eprintln!(
        "旧版本已保留为同名 .bak 文件（{}\\*.bak），确认无误后可手动删除",
        dir.display()
    );
    // 托盘因解锁被杀时负责拉起（审查 P1）：否则更新"成功"后用户托盘凭空
    // 消失——便携版没有看门狗任务，没人会替我们重启它。
    if tray_killed {
        match std::process::Command::new(dir.join("kynoptic-tray.exe"))
            .arg("--minimized")
            .spawn()
        {
            Ok(_) => {
                let _ = std::fs::remove_file(exit_flag_for(&dir));
                eprintln!("托盘已随更新自动重启");
            }
            Err(err) => warnings.push(format!(
                "托盘自动重启失败（{err}），请手动启动 kynoptic-tray.exe"
            )),
        }
    }
    for w in warnings {
        eprintln!("警告: {w}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_package_manager_paths() {
        assert_eq!(
            looks_like_package_manager_install("C:\\Users\\x\\scoop\\apps\\kynoptic\\kynoptic.exe"),
            Some("scoop update kynoptic")
        );
        assert_eq!(
            looks_like_package_manager_install("C:\\Users\\x\\.cargo\\bin\\kynoptic.exe"),
            Some("cargo install kynoptic")
        );
        assert_eq!(
            looks_like_package_manager_install("C:\\tools\\kynoptic\\kynoptic.exe"),
            None
        );
    }

    #[test]
    fn sha256_fips_vectors() {
        assert_eq!(
            sha256::hex_digest(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256::hex_digest(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha256::hex_digest(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
        // 跨块（>64 字节）+ 长度取模场景
        let long = vec![b'a'; 1000];
        assert_eq!(
            sha256::hex_digest(&long),
            "41edece42d63e8d9bf515a9ba6932e1c20cbc9f5a5d134645adb5db1b9737ea3"
        );
    }

    #[test]
    fn version_ordering() {
        use std::cmp::Ordering::*;
        assert_eq!(version_cmp("0.1.0", "0.1.0"), Equal);
        assert_eq!(version_cmp("0.2.0", "0.1.9"), Greater);
        assert_eq!(version_cmp("0.1", "0.1.0"), Less);
    }

    #[test]
    fn parses_sums_file() {
        let text = "AAAA  kynoptic.exe\nbbbb  Kynoptic-Tray.EXE\n";
        let m = parse_sums(text);
        assert_eq!(m.get("kynoptic.exe").map(String::as_str), Some("aaaa"));
        assert_eq!(m.get("kynoptic-tray.exe").map(String::as_str), Some("bbbb"));
    }

    #[test]
    fn asset_names_extend_bin_names_without_reordering() {
        // 替换顺序逻辑依赖 BIN_NAMES[0] == 主 exe；ASSET_NAMES 只能在前缀之后
        // 追加 SKILL.md，不得插入或重排三件套
        assert_eq!(ASSET_NAMES[0], BIN_NAMES[0]);
        for (i, n) in BIN_NAMES.iter().enumerate() {
            assert_eq!(ASSET_NAMES[i], *n);
        }
        assert_eq!(ASSET_NAMES[BIN_NAMES.len()], SKILL_MD_NAME);
    }

    #[test]
    fn prerelease_tag_filter_predicate() {
        // Wave16 审查：旧版测试用本地闭包复述生产逻辑，是同义反复——
        // 真实谓词被改坏时测试照绿。改为直接调用生产函数。
        assert!(is_stable_release("0.3.0"));
        assert!(!is_stable_release("0.3.0-rc.1"), "预发布必须被过滤");
        assert!(!is_stable_release("0.2.0-1"), "带后缀的 tag 被过滤（有意）");
        assert!(!is_stable_release("0.2.0-beta"), "beta 必须被过滤");
        // 修复（审查 low）：无连字符但带非数字尾缀的非法 tag 同样必须被拒
        assert!(!is_stable_release("v0.2.1x"), "非数字尾缀 tag 必须被过滤");
        assert!(!is_stable_release("0.3.0beta"), "非数字尾缀 tag 必须被过滤");
        assert!(!is_stable_release("0.2.1.1.1"), "超过三段必须被过滤");
        assert!(!is_stable_release("0..1"), "空组件必须被过滤");
        assert!(is_stable_release("v0.2.1"), "带 v 前缀的合法 tag 放行");
    }

    #[test]
    fn orphan_bak_recovery_restores_missing_exe() {
        let dir = std::env::temp_dir().join(format!("kynoptic-orphan-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // tray exe 缺失 + .bak 在位 → 应还原；完好的 exe + .bak → 不动
        std::fs::write(dir.join("kynoptic-tray.exe.bak"), b"old").unwrap();
        std::fs::write(dir.join("kynoptic.exe"), b"cur").unwrap();
        assert_eq!(recover_orphan_baks(&dir), 1);
        assert_eq!(
            std::fs::read(dir.join("kynoptic-tray.exe")).unwrap(),
            b"old"
        );
        assert!(!dir.join("kynoptic-tray.exe.bak").exists());
        assert_eq!(recover_orphan_baks(&dir), 0, "无孤儿时不应有动作");
        assert_eq!(std::fs::read(dir.join("kynoptic.exe")).unwrap(), b"cur");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
