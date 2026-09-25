//! CLI 输出文案与 db 错误脱敏的统一出口。
//!
//! 口径定稿（审查整改）：
//! - 终端面向用户输出中文正式风，不用内部表名（如 daily_agg）与 SQLite
//!   错误原文；统计口径统一用「按键 / 点击 / 活跃分钟 / 每分钟按键数」。
//! - raw 错误原文只写日志文件（exe 同目录 watchdog.log，与看门狗日志同
//!   文件，尽力而为），不回显终端——watchdog_log 会同步 eprint，故脱敏
//!   路径走本模块的 [`log_raw_only`]，只落盘不回显。

use std::path::Path;

/// 数据库不存在（读命令 / dashboard 均不建库不迁移）的双语文案。
/// role_cn/role_en：调用方角色，如「面板 / dashboard」「只读命令 / read-only commands」。
pub fn db_missing(path: &Path, role_cn: &str, role_en: &str) -> String {
    format!(
        "数据库不存在: {}（先运行一次采集器生成，{role_cn}不建库不迁移 / \
         database not found: run the collector once to create it; \
         the {role_en} does not create or migrate it）",
        path.display()
    )
}

/// 只写日志文件、不回显终端：脱敏出口的落盘通道。
/// 复用 exe 同目录 watchdog.log 与既有轮转逻辑（超 1MB 改 .old），
/// 失败忽略（日志是尽力而为语义，不影响主流程）。
fn log_raw_only(msg: &str) {
    if let Some(dir) = crate::exe_dir() {
        let log_path = dir.join(crate::WATCHDOG_LOG_FILE);
        crate::rotate_log_if_needed(&log_path);
        use std::io::Write;
        let line = format!("[{}] {}\n", chrono::Utc::now().to_rfc3339(), msg);
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
        {
            let _ = f.write_all(line.as_bytes());
        }
    }
}

/// db 操作失败的统一脱敏出口：raw 错误（含 SQLite 原文、内部表名）只进
/// 日志文件；终端只给「数据安全 + 可重试 + 详情在日志」的用户语义。
pub fn db_failure(context: &str, raw: &impl std::fmt::Display) -> String {
    log_raw_only(&format!("CLI {context}: {raw}"));
    format!("⚠ {context}，数据本身安全，可稍后重试（详细原因已写入 watchdog.log）")
}
