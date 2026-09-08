//! `kynoptic-ctl` — CLI 管理工具
//!
//! 替代 Python 时代的 `python -m kynoptic stats/export/report/dashboard/db`
//!
//! 错误类型统一为 [`kynoptic_core::Error`]（io/serde/csv/rusqlite 自动 `?`
//! 转换），main 处统一打印后返回 FAILURE。
//!
//! 子命令：
//!   stats     — 今日/指定日统计摘要
//!   export    — 导出事件为 CSV/JSON/JSONL
//!   report    — 生成每日 Markdown 报告
//!   db        — 数据库维护 (stats/cleanup/vacuum/checkpoint)
//!   analyze   — 单日完整分析 (focus/fragmentation/anomaly)
//!   ghost     — 清扫幽灵 session
//!   autostart — 开关开机自启动
//!   migrate   — 从旧 Python db 导入（已迁移到 scripts/migrate_legacy_db.py）

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use chrono::{Duration, Utc};
use rusqlite::Connection;
use serde_json::json;

use kynoptic_core::analyzer;
use kynoptic_core::anomaly;
use kynoptic_core::daily_agg;
use kynoptic_core::db::Database;
use kynoptic_core::queries;
use kynoptic_core::{Error, Result};

// 引用父 crate 的 autostart 模块
mod autostart;

/// 把 csv::Error 转为 [`Error`]：底层是 io 错误时保留为 Io，否则归为 InvalidData。
/// （csv 不在 kynoptic-core 依赖内，故在 ctl 本地做转换。）
fn map_csv_err(e: csv::Error) -> Error {
    if e.is_io_error() {
        // csv::Error 持有 io::Error 但未直接暴露；用 Display 文本兜底
        Error::Io(std::io::Error::other(e.to_string()))
    } else {
        Error::InvalidData(format!("CSV 错误: {e}"))
    }
}

const USAGE: &str = "kynoptic-ctl <subcommand> [options]

Subcommands:
  stats     [--date YYYY-MM-DD] [--days N]   Show summary stats
  export    [--days N] [--format csv|json|jsonl] [--out PATH]
  report    [--date YYYY-MM-DD] [--save PATH]   Generate Markdown report
  db        [stats|cleanup [N]|vacuum|checkpoint]   DB maintenance
  analyze   [--date YYYY-MM-DD]                Focus/fragment/anomaly report
  ghost                                       Close ghost sessions
  autostart [enable|disable|status]            Toggle auto-start
  migrate   [--legacy PATH] [--target PATH]   Migrate from legacy db
";

fn resolve_db() -> PathBuf {
    // 复用 core 的统一路径解析逻辑，与主应用保持一致（exe 同级优先，cwd 兜底）。
    kynoptic_core::db::resolve_db_path()
}

fn open_db(path: &Path) -> Result<Connection> {
    // 复用 core 的初始化逻辑（SCHEMA + 迁移 + 全套 PRAGMA），
    // 替代原只设 2 个 PRAGMA 的实现——避免 ctl 操作比应用旧的库时缺列。
    let conn = Connection::open(path)?;
    kynoptic_core::db::apply_pragmas(&conn)?;
    conn.execute_batch(kynoptic_core::db::SCHEMA)?;
    kynoptic_core::db::run_migrations(&conn);
    Ok(conn)
}

fn parse_date(s: &str) -> Result<String> {
    // 接受 YYYY-MM-DD；空 = 今天（本地时区，与 app 的 today_range 同源）
    if s.is_empty() {
        return Ok(queries::today_local_str());
    }
    chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d")
        .map(|d| d.format("%Y-%m-%d").to_string())
        .map_err(|e| Error::InvalidData(format!("日期格式错: {e}")))
}

// === stats ===
fn cmd_stats(args: &[String]) -> Result<()> {
    let mut date = String::new();
    let mut days: i64 = 1;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--date" => {
                i += 1;
                date = args.get(i).cloned().unwrap_or_default();
            }
            "--days" => {
                i += 1;
                days = args.get(i).and_then(|s| s.parse().ok()).unwrap_or(1);
            }
            _ => {}
        }
        i += 1;
    }
    let db_path = resolve_db();
    let conn = open_db(&db_path)?;
    if days == 1 {
        let date = if date.is_empty() {
            Utc::now().format("%Y-%m-%d").to_string()
        } else {
            parse_date(&date)?
        };
        // 复用 analyzer::analyze_day（消除原 substr(timestamp,1,10)=? 的 WHERE 全表扫描，
        // 并与 cmd_report/cmd_analyze 共享同一套当日聚合逻辑）
        let day = analyzer::analyze_day(&conn, &date)?;
        println!("=== Stats for {} ===", date);
        println!("keys:     {}", day.total_keys);
        println!("clicks:   {}", day.total_clicks);
        println!(
            "active:   {} min ({} h)",
            day.active_minutes,
            day.active_minutes as f64 / 60.0
        );
        println!("apm:      {:.1}", day.apm_avg);
    } else {
        println!("=== Stats for last {} days ===", days);
        println!(
            "{:<12} {:>7} {:>7} {:>7} {:>7}",
            "date", "keys", "clicks", "act_min", "apm"
        );
        for row in daily_agg::recent(&conn, days) {
            println!(
                "{:<12} {:>7} {:>7} {:>7} {:>7.1}",
                row.0, row.1, row.2, row.3, row.4
            );
        }
    }
    Ok(())
}

// === export ===
fn cmd_export(args: &[String]) -> Result<()> {
    let mut days: i64 = 7;
    let mut format = "csv".to_string();
    let mut out = String::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--days" => {
                i += 1;
                days = args.get(i).and_then(|s| s.parse().ok()).unwrap_or(7);
            }
            "--format" => {
                i += 1;
                format = args.get(i).cloned().unwrap_or_else(|| "csv".to_string());
            }
            "--out" => {
                i += 1;
                out = args.get(i).cloned().unwrap_or_default();
            }
            _ => {}
        }
        i += 1;
    }
    let cutoff = (Utc::now() - Duration::days(days)).to_rfc3339();
    let conn = open_db(&resolve_db())?;
    let rows = queries::export_events_since(&conn, &cutoff);

    if out.is_empty() {
        out = format!(
            "kynoptic_export_{}.{}",
            Utc::now().format("%Y%m%d_%H%M%S"),
            ext(&format)
        );
    }
    let out_path = PathBuf::from(&out);

    match format.as_str() {
        "csv" => {
            let mut w = csv::Writer::from_path(&out_path).map_err(map_csv_err)?;
            w.write_record([
                "id",
                "timestamp",
                "event_type",
                "event_action",
                "event_data",
                "app_name",
                "window_title",
                "session_id",
            ])
            .map_err(map_csv_err)?;
            for r in &rows {
                w.write_record(&[
                    r.id.to_string(),
                    r.timestamp.clone(),
                    r.event_type.clone(),
                    r.event_action.clone(),
                    r.event_data.clone().unwrap_or_default(),
                    r.app_name.clone().unwrap_or_default(),
                    r.window_title.clone().unwrap_or_default(),
                    r.session_id.map(|x| x.to_string()).unwrap_or_default(),
                ])
                .map_err(map_csv_err)?;
            }
            w.flush()?;
        }
        "json" => {
            let arr: Vec<_> = rows.iter().map(|r| json!({
                "id": r.id, "timestamp": r.timestamp, "event_type": r.event_type, "event_action": r.event_action,
                "event_data": r.event_data, "app_name": r.app_name, "window_title": r.window_title, "session_id": r.session_id
            })).collect();
            std::fs::write(&out_path, serde_json::to_string_pretty(&arr)?)?;
        }
        "jsonl" => {
            use std::io::Write;
            let mut f = std::fs::File::create(&out_path)?;
            for r in &rows {
                let obj = json!({
                    "id": r.id, "timestamp": r.timestamp, "event_type": r.event_type, "event_action": r.event_action,
                    "event_data": r.event_data, "app_name": r.app_name, "window_title": r.window_title, "session_id": r.session_id
                });
                writeln!(f, "{}", serde_json::to_string(&obj)?)?;
            }
        }
        other => {
            return Err(Error::InvalidData(format!(
                "未知格式: {other}（支持 csv/json/jsonl）"
            )))
        }
    }
    println!("✓ 导出 {} 行 → {}", rows.len(), out_path.display());
    Ok(())
}

fn ext(f: &str) -> &'static str {
    match f {
        "json" => "json",
        "jsonl" => "jsonl",
        _ => "csv",
    }
}

// === report ===
fn cmd_report(args: &[String]) -> Result<()> {
    let mut date = String::new();
    let mut save = String::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--date" => {
                i += 1;
                date = args.get(i).cloned().unwrap_or_default();
            }
            "--save" => {
                i += 1;
                save = args.get(i).cloned().unwrap_or_default();
            }
            _ => {}
        }
        i += 1;
    }
    let date = if date.is_empty() {
        Utc::now().format("%Y-%m-%d").to_string()
    } else {
        parse_date(&date)?
    };
    let conn = open_db(&resolve_db())?;
    let analysis = analyzer::analyze_day(&conn, &date)?;
    let anomalies = anomaly::detect_all(&conn, &date).unwrap_or_default();

    let mut md = String::new();
    md.push_str(&format!("# 数字脉搏 · {} 报告\n\n", date));
    md.push_str(&format!("- 按键: **{}**\n", analysis.total_keys));
    md.push_str(&format!("- 点击: **{}**\n", analysis.total_clicks));
    md.push_str(&format!(
        "- 活跃: **{}** 分钟 ({:.1} h)\n",
        analysis.active_minutes,
        analysis.active_minutes as f64 / 60.0
    ));
    md.push_str(&format!("- APM: **{:.1}**\n\n", analysis.apm_avg));

    md.push_str("## 专注段\n\n");
    if analysis.focus_segments.is_empty() {
        md.push_str("今天没有 ≥ 5 分钟的专注段。\n\n");
    } else {
        md.push_str("| 起 | 止 | 时长(分) | 按键 | 点击 | 应用 |\n");
        md.push_str("|---|---|---:|---:|---:|---|\n");
        for s in &analysis.focus_segments {
            md.push_str(&format!(
                "| {} | {} | {} | {} | {} | {} |\n",
                &s.start[11..16],
                &s.end[11..16],
                s.duration_min,
                s.key_count,
                s.click_count,
                s.app_name.clone().unwrap_or_else(|| "—".into())
            ));
        }
        md.push('\n');
    }

    md.push_str("## 碎片化指数\n\n");
    md.push_str(&format!(
        "- 指数: **{:.3}** (0=连续, 1=完全碎片)\n",
        analysis.fragmentation.fragmentation_index
    ));
    md.push_str(&format!(
        "- 最长连续段: **{}** 分钟\n",
        analysis.fragmentation.longest_streak_min
    ));
    md.push_str(&format!(
        "- 打断次数: **{}**\n\n",
        analysis.fragmentation.total_breaks
    ));

    md.push_str("## 异常\n\n");
    if anomalies.is_empty() {
        md.push_str("今天没有检测到异常。\n");
    } else {
        for a in &anomalies {
            md.push_str(&format!(
                "- **{}** [{}] {}\n",
                a.severity, a.kind, a.message
            ));
            md.push_str(&format!("  - {}\n", a.detail));
        }
    }

    if !save.is_empty() {
        std::fs::write(&save, &md)?;
        println!("✓ 报告已保存到 {}", save);
    } else {
        println!("{}", md);
    }
    Ok(())
}

// === db ===
fn cmd_db(args: &[String]) -> Result<()> {
    let sub = args.first().map(|s| s.as_str()).unwrap_or("stats");
    let db_path = resolve_db();
    let conn = open_db(&db_path)?;
    match sub {
        "stats" => {
            let n_events = queries::count_all_events(&conn);
            let n_sessions = queries::count_all_sessions(&conn);
            let n_ghost = queries::count_ghost_sessions(&conn);
            let (size_wal, size_main) = (
                file_size(&db_path.with_extension("db-wal")),
                file_size(&db_path),
            );
            println!("=== DB Stats ===");
            println!("path:        {}", db_path.display());
            println!("events:      {}", n_events);
            println!("sessions:    {} ({} ghost)", n_sessions, n_ghost);
            println!("main file:   {} bytes", size_main);
            println!("wal file:    {} bytes", size_wal);
        }
        "cleanup" => {
            let days: i64 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(90);
            let cutoff = (Utc::now() - Duration::days(days)).to_rfc3339();
            let n = queries::delete_events_before(&conn, &cutoff)?;
            let ns = queries::delete_closed_sessions_before(&conn, &cutoff)?;
            println!(
                "✓ 清理: 删除 {} 事件, {} sessions（保留 {} 天）",
                n, ns, days
            );
        }
        "vacuum" => {
            println!("正在 VACUUM...");
            conn.execute_batch("VACUUM")?;
            println!("✓ 完成");
        }
        "checkpoint" => {
            println!("正在 WAL checkpoint...");
            conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;
            println!("✓ 完成");
        }
        "recompute-agg" => {
            let n = daily_agg::recompute_all(&conn)?;
            println!("✓ 重算 daily_agg，影响 {} 天", n);
        }
        other => {
            return Err(Error::InvalidData(format!(
                "未知子命令: {other}（stats/cleanup/vacuum/checkpoint/recompute-agg）"
            )))
        }
    }
    Ok(())
}

fn file_size(p: &Path) -> u64 {
    std::fs::metadata(p).map(|m| m.len()).unwrap_or(0)
}

// === analyze ===
fn cmd_analyze(args: &[String]) -> Result<()> {
    let mut date = String::new();
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--date" {
            i += 1;
            date = args.get(i).cloned().unwrap_or_default();
        }
        i += 1;
    }
    let date = if date.is_empty() {
        Utc::now().format("%Y-%m-%d").to_string()
    } else {
        parse_date(&date)?
    };
    let conn = open_db(&resolve_db())?;
    let analysis = analyzer::analyze_day(&conn, &date)?;
    let anomalies = anomaly::detect_all(&conn, &date).unwrap_or_default();
    println!("=== Analyze {} ===", date);
    println!("keys:           {}", analysis.total_keys);
    println!("clicks:         {}", analysis.total_clicks);
    println!("active minutes: {}", analysis.active_minutes);
    println!("apm:            {:.1}", analysis.apm_avg);
    println!("focus segments: {}", analysis.focus_segments.len());
    for s in &analysis.focus_segments {
        println!(
            "  {} → {}  {}min  keys={} clicks={} app={:?}",
            &s.start[11..16],
            &s.end[11..16],
            s.duration_min,
            s.key_count,
            s.click_count,
            s.app_name
        );
    }
    println!(
        "fragmentation:  {:.3} (longest {} min, {} breaks)",
        analysis.fragmentation.fragmentation_index,
        analysis.fragmentation.longest_streak_min,
        analysis.fragmentation.total_breaks
    );
    println!("anomalies:      {}", anomalies.len());
    for a in &anomalies {
        println!("  [{}] {} - {}", a.severity, a.kind, a.message);
    }
    Ok(())
}

// === ghost ===
fn cmd_ghost() -> Result<()> {
    let db_path = resolve_db();
    let db = Database::open(db_path.to_str().unwrap())?;
    let n = db.close_ghost_sessions();
    println!("✓ 关闭了 {} 个 session", n);
    // 重新计算所有天的 daily_agg（让 stats/anomaly 立刻反映新数据）
    let conn = db.reader();
    match daily_agg::recompute_all(&conn) {
        Ok(d) => println!("  同步更新 daily_agg {} 天", d),
        Err(e) => eprintln!("  ⚠ daily_agg 更新失败: {e}"),
    }
    Ok(())
}

// === autostart ===
fn cmd_autostart(args: &[String]) -> Result<()> {
    let sub = args.first().map(|s| s.as_str()).unwrap_or("status");
    // 始终指向 digital-pulse.exe（同目录或 src-tauri\target\debug）
    let app_exe = locate_app_exe().unwrap_or_else(|| std::env::current_exe().unwrap_or_default());
    match sub {
        "enable" => {
            autostart::enable(&app_exe, &["--minimized"])?;
            println!("✓ 开机自启动已启用 (注册表 Run)");
            println!("  → {} --minimized", app_exe.display());
        }
        "disable" => {
            autostart::disable()?;
            println!("✓ 开机自启动已禁用");
        }
        "status" => {
            let enabled = autostart::is_enabled().unwrap_or(false);
            let cur = autostart::current().unwrap_or(None);
            println!("enabled: {}", enabled);
            println!("current: {}", cur.unwrap_or_else(|| "(none)".into()));
        }
        other => {
            return Err(Error::InvalidData(format!(
                "未知子命令: {other}（enable/disable/status）"
            )))
        }
    }
    Ok(())
}

/// 找到同目录下的 digital-pulse.exe（dev 模式）；找不到就用 ctl 自己
fn locate_app_exe() -> Option<std::path::PathBuf> {
    let ctl = std::env::current_exe().ok()?;
    let dir = ctl.parent()?;
    for cand in ["digital-pulse.exe", "digital-pulse"] {
        let p = dir.join(cand);
        if p.exists() {
            return Some(p);
        }
    }
    None
}

// === migrate (delegates to python script) ===
fn cmd_migrate(_args: &[String]) -> Result<()> {
    let script = std::env::current_dir()
        .map(|d| d.join("scripts/migrate_legacy_db.py"))
        .map_err(Error::Io)?;
    if !script.exists() {
        return Err(Error::InvalidData(format!(
            "找不到迁移脚本: {}",
            script.display()
        )));
    }
    println!("请运行: python {}", script.display());
    Ok(())
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let sub = args.first().map(|s| s.as_str()).unwrap_or("help");
    let result: Result<()> = match sub {
        "stats" => cmd_stats(&args[1..]),
        "export" => cmd_export(&args[1..]),
        "report" => cmd_report(&args[1..]),
        "db" => cmd_db(&args[1..]),
        "analyze" => cmd_analyze(&args[1..]),
        "ghost" => cmd_ghost(),
        "autostart" => cmd_autostart(&args[1..]),
        "migrate" => cmd_migrate(&args[1..]),
        "help" | "-h" | "--help" => {
            print!("{}", USAGE);
            Ok(())
        }
        other => Err(Error::InvalidData(format!(
            "未知子命令: {other}\n\n{USAGE}"
        ))),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("✗ {e}");
            ExitCode::FAILURE
        }
    }
}
