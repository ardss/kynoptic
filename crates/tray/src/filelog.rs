//! 极简文件 logger（日志与可观测性全景审查 P0 修复）：
//!
//! 此前 kynoptic-tray 从不初始化 logger——main.rs / ghost.rs / dash 里的所有
//! `log::warn!`/`log::error!` 在托盘进程里等于静默丢弃，违反项目铁律
//! "后台错误必须落文件"。collector 内部的 env_logger 只写 stderr，托盘
//! 无 console 等于同样不可见。
//!
//! 本模块提供零新依赖的自写实现（tray 已依赖 log + chrono）：
//! - 写 `<db 目录>\tray.log`，追加；
//! - 单文件 1MB 上限，超限轮转一次成 tray.log.old（覆盖式）——日志类
//!   文件必须有界，2MB 硬顶，不违反存储纪律（数据文件不删铁律只针对数据）；
//! - 写失败静默忽略（日志通道绝不能反过来弄崩托盘）；
//! - 不弹窗、无 stdout 输出。
//!
//! 初始化时机：tray main() 里尽早调用 [`init`]；此后 collector 的
//! `env_logger try_init` 会因 logger 已安装而静默让位，全部日志统一落本文件。

use log::{Level, LevelFilter, Log, Metadata, Record};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// 单文件上限：1MB，轮转一次成 .old 后重新计数（总占用 ≤ 2MB）
pub const MAX_BYTES: u64 = 1024 * 1024;

pub struct FileLogger {
    path: PathBuf,
    /// 串行化 append + rotate 检查（Mutex<()> 而非锁文件，进程内足够）
    lock: Mutex<()>,
}

impl FileLogger {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            lock: Mutex::new(()),
        }
    }

    /// 轮转目标路径（纯函数，单测覆盖）：tray.log → tray.log.old
    pub fn rotated_path(path: &Path) -> PathBuf {
        let mut s = path.as_os_str().to_owned();
        s.push(".old");
        PathBuf::from(s)
    }

    fn rotate_if_needed(&self) {
        let ok = std::fs::metadata(&self.path)
            .map(|m| m.len() >= MAX_BYTES)
            .unwrap_or(false);
        if ok {
            let old = Self::rotated_path(&self.path);
            let _ = std::fs::remove_file(&old);
            // 打开中的 append 句柄/杀毒扫描短暂持有句柄都会令 rename 失败：
            // 短重试几次；仍失败则退化为截断（宁可丢旧日志也不能让文件
            // 无限膨胀）
            let mut done = false;
            for _ in 0..5 {
                if std::fs::rename(&self.path, &old).is_ok() {
                    done = true;
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            if !done {
                let _ = std::fs::File::create(&self.path);
            }
        }
    }

    /// 直接写一条记录（测试与 init 前的引导路径复用）。
    /// 轮转检查在追加之后做（metadata 很便宜，且托盘日志频率极低）：
    /// 写完若超 1MB 立即轮转，文件不会在两次写之间长期超限。
    pub fn write_record(&self, level: Level, target: &str, args: &str) {
        let _guard = self.lock.lock();
        let line = format!(
            "{} {:<5} [{}] {}\n",
            chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f"),
            level.as_str(),
            target,
            args
        );
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            let _ = f.write_all(line.as_bytes());
        }
        self.rotate_if_needed();
    }
}

impl Log for FileLogger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= Level::Info
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        self.write_record(
            record.level(),
            record.target(),
            &format!("{}", record.args()),
        );
    }

    fn flush(&self) {}
}

/// 在 tray 启动时尽早安装。失败静默（重复 init / 已有 logger 时让位，
/// 绝不让日志通道阻塞启动）。级别固定 Info。
pub fn init(path: PathBuf) {
    if log::set_boxed_logger(Box::new(FileLogger::new(path))).is_ok() {
        log::set_max_level(LevelFilter::Info);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "kynoptic-filelog-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn warn_record_lands_in_file() {
        let dir = tmp_dir("basic");
        let log = FileLogger::new(dir.join("tray.log"));
        log.write_record(Level::Warn, "test", "维护日志可见性验证");
        let content = std::fs::read_to_string(dir.join("tray.log")).unwrap();
        assert!(content.contains("WARN"), "级别必须在行内: {content}");
        assert!(content.contains("维护日志可见性验证"), "消息必须落文件: {content}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rotates_once_past_1mb() {
        let dir = tmp_dir("rotate");
        let path = dir.join("tray.log");
        let log = FileLogger::new(path.clone());
        // 直接灌一条 >1MB 的记录触发轮转判定
        let big = "x".repeat(MAX_BYTES as usize + 16);
        log.write_record(Level::Warn, "test", &big);
        let old = FileLogger::rotated_path(&path);
        assert!(old.exists(), "超限后必须轮转出 .old");
        let old_content = std::fs::read_to_string(&old).unwrap();
        assert!(old_content.contains("WARN"), "超限记录本体必须在 .old 里");
        assert!(
            !path.exists() || std::fs::metadata(&path).unwrap().len() < MAX_BYTES,
            "当前 tray.log 必须回到上限以内（不存在 = 轮转后尚无新写入）"
        );
        // 轮转后继续写必须落到新 tray.log
        log.write_record(Level::Warn, "test", "轮转后写入");
        let fresh = std::fs::read_to_string(&path).unwrap();
        assert!(fresh.contains("轮转后写入"), "轮转后新文件仍要能继续写");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rotated_path_appends_old_suffix() {
        assert_eq!(
            FileLogger::rotated_path(Path::new("C:\\data\\tray.log")),
            PathBuf::from("C:\\data\\tray.log.old")
        );
    }
}
