//! 子进程创建旗标防漏闸（R10）：源码扫描测试。
//!
//! 背景：core::spawn 的「一切控制台类子进程统一过 no_window 旗标」契约此前
//! 只存在于文档注释，无机械强制——W42 起多轮闪框修复均因漏点复发，#29 统一轮
//! 的负向 grep（「no scattered creation_flags remain」）对从未写过旗标的裸
//! `Command::new` 站点结构性不可见（cli update.rs:1009、cli main.rs:1470 即
//! 如此漏扫）。本测试把契约变成机械断言：非测试产品代码中每处
//! `Command::new`，从该行到首个 `.spawn()`/`.status()`/`.output()` 终结调用
//! 之间必须出现 `spawn::no_window(` 或 `quiet_command(`。
//!
//! 豁免白名单（显式列举，新增豁免须在此留理由注释）：
//! - tests/、examples/、benches/ 目录：测试与交互式开发工具，测试期本就有
//!   开发者可见的控制台；
//! - cli/src/bin/kynoptic-watchdog.rs：仓库唯一 GUI 子系统进程（windows_subsystem
//!   声明），其子进程无控制台可闪；
//! - `#[cfg(test)]` 模块与 `#[test]` 函数（含 `#[cfg(windows)] #[test]` 形态）：
//!   源码内联测试代码，做花括号配平剥离。
//!
//! 已知局限（可接受）：剥离与邻近判定按行做朴素配平/窗口扫描，极端的注释
//! 花括号可能误剥离——后果是误报可见（会当轮被抓），不会静默放过漏点。

use std::path::{Path, PathBuf};

/// 从 crates/core 定位仓库根（CARGO_MANIFEST_DIR = <root>/crates/core）。
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("CARGO_MANIFEST_DIR 必为 <root>/crates/core 形态")
        .to_path_buf()
}

fn collect_rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .expect("src 目录必须存在")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .collect();
    entries.sort();
    for p in entries {
        if p.is_dir() {
            collect_rs_files(&p, out);
        } else if p.extension().map(|e| e == "rs").unwrap_or(false) {
            out.push(p);
        }
    }
}

/// 路径是否落入豁免白名单目录（tests/examples/benches）。
fn exempt_dir(p: &Path) -> bool {
    p.components().any(|c| {
        let s = c.as_os_str().to_string_lossy();
        s == "tests" || s == "examples" || s == "benches"
    })
}

/// 剥离 `#[cfg(test)]` 模块与 `#[test]`/`#[cfg(test)]` 属性直接修饰的项
/// （含 `#[cfg(windows)] #[test]` 组合形态）：自修饰项首行起做花括号配平删除。
fn strip_test_code(src: &str) -> String {
    let lines: Vec<&str> = src.lines().collect();
    let mut out: Vec<&str> = Vec::new();
    let mut i = 0usize;
    while i < lines.len() {
        let t = lines[i].trim_start();
        if t == "#[cfg(test)]" || t == "#[test]" {
            // 找属性组修饰的项首行（跳过中间其他属性行）
            let mut j = i;
            let mut item = None;
            while j < lines.len() {
                let tj = lines[j].trim_start();
                if tj.starts_with("#[") {
                    j += 1;
                    continue;
                }
                if tj.starts_with("mod ")
                    || tj.starts_with("fn ")
                    || tj.starts_with("pub ")
                    || tj.starts_with("async ")
                {
                    item = Some(j);
                }
                break;
            }
            if let Some(j) = item {
                let mut depth: i64 = 0;
                let mut started = false;
                let mut k = j;
                while k < lines.len() {
                    depth += lines[k].matches('{').count() as i64;
                    depth -= lines[k].matches('}').count() as i64;
                    if depth > 0 {
                        started = true;
                    }
                    if started && depth <= 0 {
                        break;
                    }
                    k += 1;
                }
                i = k + 1;
                continue;
            }
        }
        out.push(lines[i]);
        i += 1;
    }
    out.join("\n")
}

/// 单文件扫描：返回 (行号, 行内容) 违规列表。
/// 判定：`Command::new` 行到首个 `.spawn(`/`.status(`/`.output(` 终结调用
/// 之间必须出现 `no_window(` 或 `quiet_command(`；向前最多看 30 行兜底。
fn scan_file(stripped: &str) -> Vec<(usize, String)> {
    let lines: Vec<&str> = stripped.lines().collect();
    let mut violations = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if !line.contains("Command::new") {
            continue;
        }
        let mut ok = false;
        for w in lines[i..(i + 30).min(lines.len())].iter() {
            let t = w.trim_start();
            // 终结调用（真正启动/等待子进程）先于旗标出现 → 未过收口
            if t.contains(".spawn(") || t.contains(".status(") || t.contains(".output(") {
                break;
            }
            if t.contains("no_window(") || t.contains("quiet_command(") {
                ok = true;
                break;
            }
        }
        if !ok {
            violations.push((i + 1, line.trim().to_string()));
        }
    }
    violations
}

#[test]
fn no_window_scan_covers_all_product_command_new() {
    let root = workspace_root();
    let mut violations: Vec<String> = Vec::new();
    for crate_name in ["core", "cli", "tray", "dash", "mcp"] {
        let src = root.join("crates").join(crate_name).join("src");
        if !src.is_dir() {
            continue;
        }
        let mut files = Vec::new();
        collect_rs_files(&src, &mut files);
        for f in files {
            let p = f.to_string_lossy().replace('\\', "/");
            // 白名单（显式列举）：测试/示例/基准目录；watchdog 为仓库唯一
            // GUI 子系统进程（windows_subsystem 声明），其子进程无控制台可闪
            if exempt_dir(&f) || p.ends_with("cli/src/bin/kynoptic-watchdog.rs") {
                continue;
            }
            let src_text = std::fs::read_to_string(&f).expect("源码文件必须可读");
            let stripped = strip_test_code(&src_text);
            for (line_no, text) in scan_file(&stripped) {
                violations.push(format!("{p}:{line_no}: {text}"));
            }
        }
    }
    assert!(
        violations.is_empty(),
        "以下产品代码 Command::new 站点未过 spawn::no_window/quiet_command 单一收口\n\
         （契约见 crates/core/src/spawn.rs 模块文档；新增豁免须在本文件白名单留理由）：\n{}",
        violations.join("\n")
    );
}
