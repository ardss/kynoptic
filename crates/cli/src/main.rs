// `kynoptic-ctl` — CLI 管理工具
//
// 替代 Python 时代的 `python -m kynoptic stats/export/report/dashboard/db`
//
// 错误类型统一为 [`kynoptic_core::Error`]（io/serde/csv/rusqlite 自动 `?`
// 转换），main 处统一打印后返回 FAILURE。
//
// 子命令：
//   stats     — 今日/指定日统计摘要
//   export    — 导出事件为 CSV/JSON/JSONL
//   report    — 生成每日 Markdown 报告
//   db        — 数据库维护 (stats/cleanup/vacuum/checkpoint)
//   analyze   — 单日完整分析 (focus/fragmentation/anomaly)
//   ghost     — 清扫幽灵 session
//   autostart — 开关开机自启动
//   migrate   — 从旧 Python db 导入（已迁移到 scripts/migrate_legacy_db.py）
//   now       — 当前机器状态（compact 表格 / --json）
//   query     — 时间范围事件查询 (--from/--to/--bucket/--limit/--json)
//   mcp       — 启动 MCP server（stdio，Claude Desktop: command "kynoptic" args ["mcp"]）

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use chrono::{Duration, Utc};
use rusqlite::{params, Connection};
use serde_json::json;

use kynoptic_core::analyzer;
use kynoptic_core::anomaly;
use kynoptic_core::daily_agg;
use kynoptic_core::db::Database;
use kynoptic_core::queries;
use kynoptic_core::{Error, Result};

// 引用父 crate 的 autostart 模块
mod autostart;
mod dashboard;
mod settings;
mod update;

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
  collect   [--db PATH] [--all]             Run the collector (Ctrl+C to stop)
  stats     [--date YYYY-MM-DD] [--days N]   Show summary stats
  export    [--days N] [--format csv|json|jsonl] [--out PATH] [--raw]
  report    [--date YYYY-MM-DD] [--save PATH]   Generate Markdown report
  db        [stats|cleanup [N]|vacuum|checkpoint]   DB maintenance
  analyze   [--date YYYY-MM-DD]                Focus/fragment/anomaly report
  ghost                                       Close ghost sessions
  autostart [enable|disable|status]            Toggle auto-start
  migrate   [--legacy PATH] [--target PATH]   Migrate from legacy db
  now       [--json]                          Current machine status (compact)
  query     [--from T] [--to T] [--bucket B] [--limit N] [--json]
                                              Event query in time range
  mcp                                         Run MCP server over stdio
  probe     [--monitor ID] [--secs N] [--all] Live per-monitor hardware probe
  dashboard [--port N] [--db PATH]          Local-only read-only web dashboard
  update                                      Self-update from GitHub releases
  watchdog [--db PATH] [--once]             Ensure tray is alive (for Task Scheduler)
  presence  [--days N]                      Daily presence/automation/foreground summary
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
    let _ = kynoptic_core::db::run_migrations(&conn); // schema 建库路径已含迁移；此处仅兜底
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
            queries::today_local_str()
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
    let mut raw = false;
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
            // --raw：保留 window_title 原文（默认脱敏：剥离含 http 片段的 URL 查询串）
            "--raw" => raw = true,
            _ => {}
        }
        i += 1;
    }
    let db_path = resolve_db();
    let cutoff = (Utc::now() - Duration::days(days)).to_rfc3339();
    let conn = open_db(&db_path)?;
    // 流式导出（审查 P2：不再把全表载入内存）
    use std::sync::atomic::{AtomicUsize, Ordering as AOrdering};
    let row_count = std::sync::Arc::new(AtomicUsize::new(0));

    if out.is_empty() {
        // 缺省写到 db 同目录 exports/ 子目录（自动创建），不再落 CWD。
        let dir = default_export_dir(&db_path);
        std::fs::create_dir_all(&dir)?;
        out = dir
            .join(format!(
                "kynoptic_export_{}.{}",
                Utc::now().format("%Y%m%d_%H%M%S"),
                ext(&format)
            ))
            .to_string_lossy()
            .into_owned();
    }
    let out_path = PathBuf::from(&out);

    // window_title 默认脱敏（--raw 保留原文）
    let title_out = |t: &Option<String>| -> String {
        let t = t.clone().unwrap_or_default();
        if raw {
            t
        } else {
            sanitize_window_title(&t)
        }
    };

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
            let rc = row_count.clone();
            queries::export_events_since_stream(&conn, &cutoff, |r| {
                rc.fetch_add(1, AOrdering::Relaxed);
                // 外部可控字段（app_name/window_title/event_data）做公式注入中和
                let _ = w.write_record(&[
                    r.id.to_string(),
                    r.timestamp.clone(),
                    r.event_type.clone(),
                    r.event_action.clone(),
                    csv_cell(&r.event_data.unwrap_or_default()),
                    csv_cell(&r.app_name.unwrap_or_default()),
                    csv_cell(&title_out(&r.window_title)),
                    r.session_id.map(|x| x.to_string()).unwrap_or_default(),
                ]);
            });
            w.flush()?;
        }
        "json" => {
            use std::io::Write;
            let mut f = std::fs::File::create(&out_path)?;
            let rc = row_count.clone();
            let mut first = true;
            writeln!(f, "[")?;
            queries::export_events_since_stream(&conn, &cutoff, |r| {
                let n = rc.fetch_add(1, AOrdering::Relaxed);
                let obj = json!({
                    "id": r.id, "timestamp": r.timestamp, "event_type": r.event_type, "event_action": r.event_action,
                    "event_data": r.event_data, "app_name": r.app_name, "window_title": title_out(&r.window_title), "session_id": r.session_id
                });
                let line = serde_json::to_string_pretty(&obj).unwrap_or_default();
                if n > 0 {
                    writeln!(f, ",").ok();
                }
                // 缩进对齐首行
                for (i2, l) in line.lines().enumerate() {
                    if i2 == 0 {
                        write!(f, "  {l}").ok();
                    } else {
                        writeln!(f).ok();
                        write!(f, "  {l}").ok();
                    }
                }
                first = false;
                let _ = first;
            });
            writeln!(f).ok();
            write!(f, "]").ok();
        }
        "jsonl" => {
            use std::io::Write;
            let mut f = std::fs::File::create(&out_path)?;
            let rc = row_count.clone();
            queries::export_events_since_stream(&conn, &cutoff, |r| {
                rc.fetch_add(1, AOrdering::Relaxed);
                let obj = json!({
                    "id": r.id, "timestamp": r.timestamp, "event_type": r.event_type, "event_action": r.event_action,
                    "event_data": r.event_data, "app_name": r.app_name, "window_title": title_out(&r.window_title), "session_id": r.session_id
                });
                let _ = writeln!(f, "{}", serde_json::to_string(&obj).unwrap_or_default());
            });
        }
        other => {
            return Err(Error::InvalidData(format!(
                "未知格式: {other}（支持 csv/json/jsonl）"
            )))
        }
    }
    println!(
        "✓ 导出 {} 行 → {}",
        row_count.load(AOrdering::Relaxed),
        out_path.display()
    );
    Ok(())
}

/// 导出缺省目录：db 同目录的 exports/ 子目录（db 无父目录时落 cwd 的 exports/）。
fn default_export_dir(db_path: &Path) -> PathBuf {
    db_path
        .parent()
        .map(|d| d.join("exports"))
        .unwrap_or_else(|| PathBuf::from("exports"))
}

/// URL 查询串剥离（默认脱敏）：仅处理含 "http" 的片段，去掉第一个 `?`
/// 及其后的全部内容（查询串常带 token/session id 等敏感参数）。
fn sanitize_url_query(token: &str) -> String {
    if token.contains("http") {
        match token.find('?') {
            Some(i) => token[..i].to_string(),
            None => token.to_string(),
        }
    } else {
        token.to_string()
    }
}

/// window_title 默认脱敏：按空白分片，只对含 http 的片段剥查询串，
/// 其余片段原样保留。
fn sanitize_window_title(title: &str) -> String {
    title
        .split_whitespace()
        .map(sanitize_url_query)
        .collect::<Vec<_>>()
        .join(" ")
}

fn ext(f: &str) -> &'static str {
    match f {
        "json" => "json",
        "jsonl" => "jsonl",
        _ => "csv",
    }
}

/// Markdown 表格单元格转义：`|` 会破坏列结构，换行会破坏行结构。
fn md_cell(s: &str) -> String {
    s.replace('|', "\\|").replace(['\r', '\n'], " ")
}

/// 终端输出过滤 C0 控制字符（保留 \n）：防止窗口标题里的控制符
/// 污染终端/伪造输出。
fn strip_c0(s: &str) -> String {
    s.chars().filter(|&c| c == '\n' || c >= '\u{20}').collect()
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
        queries::today_local_str()
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
                md_cell(&s.app_name.clone().unwrap_or_else(|| "—".into()))
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
            // 铁律（审查 P0）：原始 events 永不删——默认只清 sessions；
            // 确要删事件必须显式 `cleanup <days> --yes` 且 days>=30。
            // 参数解析失败一律报错，不再静默落 90（曾致 cleanup 0 / -1 /
            // abc 全部变成危险的全量删除）。
            let mut with_events = false;
            let mut days_arg: Option<i64> = None;
            for a in &args[1..] {
                match a.as_str() {
                    "--yes" => with_events = true,
                    v => {
                        days_arg =
                            Some(v.parse().map_err(|_| {
                                Error::InvalidData(format!("cleanup: 无效天数 {v:?}"))
                            })?);
                    }
                }
            }
            let days = days_arg.unwrap_or(90);
            if with_events && days < 30 {
                return Err(Error::InvalidData(
                    "删除原始事件被拒绝: 天数必须 >= 30 且显式带 --yes".into(),
                ));
            }
            let cutoff = (Utc::now() - Duration::days(days)).to_rfc3339();
            let ns = queries::delete_closed_sessions_before(&conn, &cutoff)?;
            let n = if with_events {
                queries::delete_events_before(&conn, &cutoff)?
            } else {
                0
            };
            println!(
                "✓ 清理: 删除 {} 事件, {} sessions（保留 {} 天；原始事件默认保留）",
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
        queries::today_local_str()
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
            s.app_name.as_deref().map(strip_c0)
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
        println!(
            "  [{}] {} - {}",
            a.severity,
            a.kind,
            strip_c0(&a.message)
        );
    }
    Ok(())
}

// === ghost ===
fn cmd_ghost() -> Result<()> {
    let db_path = resolve_db();
    let db = Database::open(db_path.to_str().unwrap_or("?"))?;
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

// === now ===

/// `kynoptic now`：当前机器状态一行式视图（与 MCP get_current_status 同数据面）。
fn cmd_now(args: &[String]) -> Result<()> {
    let as_json = args.iter().any(|a| a == "--json");
    let conn = open_db(&resolve_db())?;
    let status = kynoptic_mcp::state::current_status(&conn, None).map_err(Error::InvalidData)?;
    if as_json {
        println!("{}", serde_json::to_string_pretty(&status)?);
        return Ok(());
    }
    let obj = status
        .as_object()
        .ok_or_else(|| Error::InvalidData("状态非对象".into()))?;
    println!("=== Kynoptic now ({}) ===", queries::today_local_str());
    for (k, v) in obj {
        println!("{k:<16} {v}");
    }
    Ok(())
}

// === query ===

/// 解析 `--from`/`--to` 时间边界。支持：
/// - `today` / `yesterday`（本地日界）
/// - `YYYY-MM-DD`（本地日的起点；作为上界时为次日零点，即闭开区间语义）
/// - RFC3339 / `YYYY-MM-DDTHH:MM`（原样使用，非 RFC3339 时补 `:00+00:00`）
fn parse_when(s: &str, is_upper_bound: bool) -> Result<String> {
    let shift = if is_upper_bound { 1 } else { 0 };
    match s {
        "today" => {
            let (start, end) = queries::today_range();
            Ok(if is_upper_bound { end } else { start })
        }
        "yesterday" => {
            let d = queries::date_offset_str(-1 + shift);
            queries::local_day_range(&d)
                .map(|(start, _)| start)
                .ok_or_else(|| Error::InvalidData(format!("无法解析时间: {s}")))
        }
        _ => {
            if let Some((start, end)) = queries::local_day_range(s) {
                // 纯日期：下界取日始，上界取次日零点（[from, to) 闭开区间）
                return Ok(if is_upper_bound { end } else { start });
            }
            if chrono::DateTime::parse_from_rfc3339(s).is_ok() {
                return Ok(s.to_string());
            }
            for fmt in ["%Y-%m-%dT%H:%M:%S", "%Y-%m-%dT%H:%M"] {
                if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(s, fmt) {
                    // 用户输入的是本地钟表时间（审查 P2：按 UTC 补偏移会错 8 小时）
                    use chrono::TimeZone;
                    if let Some(local) = chrono::Local.from_local_datetime(&dt).single() {
                        return Ok(local.to_rfc3339());
                    }
                    return Ok(format!("{}+00:00", dt.format("%Y-%m-%dT%H:%M:%S")));
                }
            }
            Err(Error::InvalidData(format!(
                "无法解析时间: {s}（支持 today/yesterday/YYYY-MM-DD/RFC3339）"
            )))
        }
    }
}

/// bucket 过滤：接受 bucket id（`activity/keys`、`app/window`、`system/*`…）
/// 或裸 event_type（`keyboard`…）。v0.1 events 表仍是 legacy 布局（见 CODE_NOTES §2），
/// bucket 的第二段（监控器名）暂不参与过滤。
fn parse_bucket(s: &str) -> Result<String> {
    const KNOWN: &[&str] = &[
        "keyboard",
        "mouse",
        "window",
        "system",
        "clipboard",
        "network",
        "session",
        "device",
        "location",
    ];
    // bucket id → event_type 的映射（v0.1 实际记录的采集类别）。
    // 先查完整 id（activity 段下 keys/mouse 分属两类），再退回第一段。
    const EXACT: &[(&str, &str)] = &[("activity/keys", "keyboard"), ("activity/mouse", "mouse")];
    const HEADS: &[(&str, &str)] = &[
        ("app", "window"),
        ("system", "system"),
        ("network", "network"),
        ("session", "session"),
        ("device", "device"),
    ];
    if let Some((_, etype)) = EXACT.iter().find(|(b, _)| *b == s) {
        return Ok(etype.to_string());
    }
    let head = s.split('/').next().unwrap_or(s);
    if KNOWN.contains(&head) {
        return Ok(head.to_string());
    }
    if let Some((_, etype)) = HEADS.iter().find(|(b, _)| *b == head) {
        return Ok(etype.to_string());
    }
    Err(Error::InvalidData(format!(
        "未知 bucket: {s}（允许: activity/keys, activity/mouse, app/window, system/*, network/*, session/*, device/* 或裸 event_type）"
    )))
}

struct QueryArgs {
    from: Option<String>,
    to: Option<String>,
    bucket: Option<String>,
    limit: usize,
    json: bool,
}

fn parse_query_args(args: &[String]) -> Result<QueryArgs> {
    let mut q = QueryArgs {
        from: None,
        to: None,
        bucket: None,
        limit: 50,
        json: false,
    };
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--from" => {
                i += 1;
                q.from = args.get(i).cloned();
            }
            "--to" => {
                i += 1;
                q.to = args.get(i).cloned();
            }
            "--bucket" => {
                i += 1;
                q.bucket = args.get(i).cloned();
            }
            "--limit" => {
                i += 1;
                q.limit = args
                    .get(i)
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(50)
                    .clamp(1, 1000);
            }
            "--json" => q.json = true,
            other => {
                return Err(Error::InvalidData(format!(
                    "未知选项: {other}（支持 --from/--to/--bucket/--limit/--json）"
                )))
            }
        }
        i += 1;
    }
    Ok(q)
}

/// `kynoptic query --from yesterday --bucket keyboard --limit 20 --json`
fn cmd_query(args: &[String]) -> Result<()> {
    let q = parse_query_args(args)?;
    let from = match &q.from {
        Some(s) if !s.is_empty() => parse_when(s, false)?,
        _ => parse_when("today", false)?,
    };
    let to = match &q.to {
        Some(s) if !s.is_empty() => Some(parse_when(s, true)?),
        _ => None,
    };
    let bucket = q
        .bucket
        .as_deref()
        .filter(|s| !s.is_empty())
        .map(parse_bucket)
        .transpose()?;

    let conn = open_db(&resolve_db())?;
    let sql = match (&bucket, &to) {
        (Some(_), Some(_)) => "SELECT timestamp, event_type, event_action, COALESCE(app_name,''), COALESCE(window_title,'') FROM events WHERE timestamp >= ? AND timestamp < ? AND event_type = ? ORDER BY timestamp DESC LIMIT ?",
        (Some(_), None) => "SELECT timestamp, event_type, event_action, COALESCE(app_name,''), COALESCE(window_title,'') FROM events WHERE timestamp >= ? AND event_type = ? ORDER BY timestamp DESC LIMIT ?",
        (None, Some(_)) => "SELECT timestamp, event_type, event_action, COALESCE(app_name,''), COALESCE(window_title,'') FROM events WHERE timestamp >= ? AND timestamp < ? ORDER BY timestamp DESC LIMIT ?",
        (None, None) => "SELECT timestamp, event_type, event_action, COALESCE(app_name,''), COALESCE(window_title,'') FROM events WHERE timestamp >= ? ORDER BY timestamp DESC LIMIT ?",
    };
    let mut stmt = conn.prepare(sql)?;
    let map_row =
        |r: &rusqlite::Row<'_>| -> rusqlite::Result<(String, String, String, String, String)> {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        };
    let rows: Vec<(String, String, String, String, String)> = match (&bucket, &to) {
        (Some(b), Some(t)) => stmt
            .query_map(params![from, t, b, q.limit as i64], map_row)?
            .flatten()
            .collect(),
        (Some(b), None) => stmt
            .query_map(params![from, b, q.limit as i64], map_row)?
            .flatten()
            .collect(),
        (None, Some(t)) => stmt
            .query_map(params![from, t, q.limit as i64], map_row)?
            .flatten()
            .collect(),
        (None, None) => stmt
            .query_map(params![from, q.limit as i64], map_row)?
            .flatten()
            .collect(),
    };

    if q.json {
        let arr: Vec<_> = rows
            .iter()
            .map(|(ts, t, a, app, title)| {
                json!({
                    "timestamp": ts, "event_type": t, "event_action": a,
                    "app_name": app, "window_title": title,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&arr)?);
    } else {
        println!(
            "=== {} events (from {}, limit {}) ===",
            rows.len(),
            from,
            q.limit
        );
        println!(
            "{:<25} {:<9} {:<12} {:<16} title",
            "timestamp", "type", "action", "app"
        );
        for (ts, t, a, app, title) in &rows {
            println!(
                "{ts:<25} {t:<9} {a:<12} {:<16} {}",
                truncate_col(app, 16),
                truncate_col(title, 40)
            );
        }
    }
    Ok(())
}

fn truncate_col(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max.saturating_sub(1)).collect::<String>() + "…"
    }
}

// === mcp ===

/// `kynoptic mcp`：启动 MCP server（stdio JSON-RPC，阻塞到 stdin 关闭）。
fn cmd_mcp() -> Result<()> {
    kynoptic_mcp::serve_stdio();
    Ok(())
}

// === probe ===

/// `kynoptic-ctl probe [--monitor ID] [--secs N] [--all]`
/// 实机探针：逐监控器启用、临时库采集 N 秒，报告事件数与 PASS/FAIL。
fn cmd_probe(args: &[String]) -> Result<()> {
    let mut monitor = String::new();
    let mut secs: u64 = 15;
    let mut all = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--monitor" => {
                i += 1;
                monitor = args.get(i).cloned().unwrap_or_default();
            }
            "--secs" => {
                i += 1;
                secs = args.get(i).and_then(|s| s.parse().ok()).unwrap_or(15);
            }
            "--all" => all = true,
            other => {
                return Err(Error::InvalidData(format!(
                    "未知选项: {other}（支持 --monitor/--secs/--all）"
                )))
            }
        }
        i += 1;
    }
    if all {
        let outcomes = kynoptic_core::probe::probe_all(secs);
        kynoptic_core::probe::print_matrix(&outcomes);
    } else {
        if monitor.is_empty() {
            return Err(Error::InvalidData(
                "probe 需要 --monitor ID 或 --all".into(),
            ));
        }
        let out = kynoptic_core::probe::probe_monitor(&monitor, secs);
        println!(
            "{} | dep={} | default={} | events={}",
            out.id, out.dep, out.default_enabled, out.events
        );
        println!("sample: {}", out.sample);
        for w in &out.warnings {
            println!("log: {w}");
        }
        println!(
            "verdict: {}{}",
            out.verdict.as_str(),
            if out.note.is_empty() {
                String::new()
            } else {
                format!(" ({})", out.note)
            }
        );
    }
    Ok(())
}

/// `collect` 子命令：前台运行采集器（默认 14 监控器；--all 启用全部 40 个），
/// Ctrl+C 优雅关停并打印本次会话统计。v0.1 的"跑起来"入口。
fn cmd_collect(args: &[String]) -> Result<()> {
    use kynoptic_core::collector;
    use std::sync::atomic::Ordering;

    let mut db_path = resolve_db().to_string_lossy().to_string();
    let mut all = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--db" if i + 1 < args.len() => {
                i += 1;
                db_path = args[i].clone();
            }
            "--all" => all = true,
            other => return Err(Error::InvalidData(format!("collect: 未知参数 {other}"))),
        }
        i += 1;
    }

    // 设置面板（dashboard /api/settings）写入的 settings.json 是监控器开关
    // 的唯一事实源：collect 启动时读取；--all 显式覆盖为全集。
    let enabled: std::collections::HashSet<String> = if all {
        kynoptic_core::registry::all_monitor_ids()
            .iter()
            .map(|s| s.to_string())
            .collect()
    } else {
        crate::settings::load(std::path::Path::new(&db_path))
            .enabled_monitors
            .into_iter()
            .collect()
    };
    eprintln!(
        "kynoptic collect: {} monitors, db = {db_path}. Ctrl+C to stop.",
        enabled.len()
    );
    let input_counts_only = crate::settings::load(std::path::Path::new(&db_path)).input_counts_only;
    let vk_frequency_enabled =
        crate::settings::load(std::path::Path::new(&db_path)).vk_frequency_enabled;
    let csettings = collector::CollectorSettings {
        input_granularity: if input_counts_only {
            collector::InputGranularity::Minute
        } else {
            collector::InputGranularity::Raw
        },
        // vk 频次开关：settings.json → CollectorSettings → input_agg::set_vk_enabled
        vk_frequency_enabled,
        ..collector::CollectorSettings::default()
    };
    let mut c = collector::start_collection_custom(&enabled, csettings, &db_path);

    // Ctrl+C → 优雅关停，保证缓冲事件 flush、session 正常关闭
    let shutdown = c.shutdown_flag().clone();
    ctrlc::set_handler(move || {
        shutdown.store(true, std::sync::atomic::Ordering::Release);
    })
    .map_err(|e| Error::InvalidData(format!("ctrl-c handler: {e}")))?;

    // 阻塞等待 writer 退出（shutdown 后 join 返回）
    c.wait();
    let n = c.total_written.load(Ordering::Relaxed);
    println!("collected {n} events into {db_path}");
    Ok(())
}

// === presence ===

/// 解析 `presence [--days N]` 参数。默认 1 天；N 非法或 < 1 报错（不静默兜底）。
fn parse_presence_args(args: &[String]) -> Result<i64> {
    let mut days: i64 = 1;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--days" => {
                i += 1;
                let raw = args.get(i).cloned().unwrap_or_default();
                days = raw
                    .parse()
                    .map_err(|_| Error::InvalidData(format!("presence: 无效天数 {raw:?}")))?;
            }
            other => {
                return Err(Error::InvalidData(format!(
                    "presence: 未知选项 {other:?}（支持 --days N）"
                )))
            }
        }
        i += 1;
    }
    if days < 1 {
        return Err(Error::InvalidData("presence: 天数必须 >= 1".into()));
    }
    Ok(days)
}

/// 桥接计数：排序去重后的分钟序列里，相邻间隙 <= gap 分钟按"无输入阅读"
/// 桥接成连续在场段，返回覆盖的分钟总数。与 dash 的 bridge_count 同逻辑。
fn bridge_count(sorted_minutes: &[i64], gap: i64) -> i64 {
    if sorted_minutes.is_empty() {
        return 0;
    }
    let mut total = 1i64;
    let mut prev = sorted_minutes[0];
    for &m in &sorted_minutes[1..] {
        let step = (m - prev).min(gap + 1);
        total += step.max(1);
        prev = m;
    }
    total
}

// TODO: 与 dash 的实现（crates/dash/src/lib.rs api_overview）下沉到 core 收敛为单一实现
/// 单日三指标（本地日界 [start, end)，RFC3339 字符串比较）：
///   人在场 = 非注入输入分钟（keys - injected_keys > 0）+ <=bridge 分钟桥接
///   自动化 = 有注入输入（injected_keys > 0）的分钟数
///   前台   = window/switch 事件间隔推算的累计分钟（sum/60）
fn presence_metrics(
    conn: &Connection,
    start: &str,
    end: &str,
    bridge_min: u32,
) -> Result<(i64, i64, i64)> {
    use chrono::Timelike;
    let minute_of_day = |minute_str: &str| -> Option<i64> {
        chrono::DateTime::parse_from_rfc3339(&format!("{}:00+00:00", minute_str))
            .ok()
            .map(|t| {
                let l = t.with_timezone(&chrono::Local);
                l.hour() as i64 * 60 + l.minute() as i64
            })
    };
    let mut human: Vec<i64> = Vec::new();
    let mut automation: Vec<i64> = Vec::new();
    if let Ok(mut stmt) = conn.prepare(
        "SELECT substr(timestamp,1,16), \
                MAX(COALESCE(json_extract(event_data,'$.keys'),0) - COALESCE(json_extract(event_data,'$.injected_keys'),0)), \
                MAX(COALESCE(json_extract(event_data,'$.injected_keys'),0)) \
             FROM events WHERE event_action = 'input_agg' AND json_valid(event_data) \
               AND timestamp >= ?1 AND timestamp < ?2 \
             GROUP BY substr(timestamp,1,16)",
    ) {
        let rows = stmt.query_map(params![start, end], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?))
        })?;
        for (minute_str, human_keys, injected) in rows.flatten() {
            if human_keys > 0 {
                if let Some(m) = minute_of_day(&minute_str) {
                    human.push(m);
                }
            } else if injected > 0 {
                if let Some(m) = minute_of_day(&minute_str) {
                    automation.push(m);
                }
            }
        }
    }
    // raw 模式（opt-in 逐键）：press/click 无法区分注入，按人算
    if let Ok(mut stmt) = conn.prepare(
        "SELECT DISTINCT substr(timestamp,1,16) FROM events \
          WHERE event_action IN ('press','click') \
            AND timestamp >= ?1 AND timestamp < ?2",
    ) {
        let rows = stmt.query_map(params![start, end], |r| r.get::<_, String>(0))?;
        for minute_str in rows.flatten() {
            if let Some(m) = minute_of_day(&minute_str) {
                human.push(m);
            }
        }
    }
    human.sort_unstable();
    human.dedup();
    automation.sort_unstable();
    automation.dedup();
    let presence = bridge_count(&human, (bridge_min.min(15)) as i64);

    // 前台分钟：窗口切换间隔累计（与 dash api_overview 同口径）
    let mut fg_secs: i64 = 0;
    if let Ok(mut stmt) = conn.prepare(
        "SELECT timestamp FROM events \
          WHERE event_type = 'window' AND event_action = 'switch' \
            AND timestamp >= ?1 AND timestamp < ?2 ORDER BY timestamp",
    ) {
        let rows = stmt.query_map(params![start, end], |r| r.get::<_, String>(0))?;
        let stamps: Vec<String> = rows.flatten().collect();
        let parse = |t: &str| chrono::DateTime::parse_from_rfc3339(t).ok();
        for pair in stamps.windows(2) {
            if let (Some(a), Some(b)) = (parse(&pair[0]), parse(&pair[1])) {
                fg_secs += (b - a).num_seconds();
            }
        }
    }
    Ok((presence, automation.len() as i64, fg_secs / 60))
}

/// `kynoptic presence [--days N]`：每日 人在场/自动化/前台 三行式摘要。
/// 口径与 dashboard 三指标一致（见 presence_metrics 注释与 TODO）。
fn cmd_presence(args: &[String]) -> Result<()> {
    let days = parse_presence_args(args)?;
    let bridge = kynoptic_dash::settings::load(&resolve_db()).presence_bridge_minutes;
    let conn = open_db(&resolve_db())?;
    println!("=== Presence last {days} day(s) (bridge <= {bridge} min) ===");
    for offset in (1 - days)..=0 {
        let date = queries::date_offset_str(offset);
        let Some((start, end)) = queries::local_day_range(&date) else {
            continue;
        };
        let (presence, automation, foreground) = presence_metrics(&conn, &start, &end, bridge)?;
        println!("{date} presence:   {presence} min");
        println!("{date} automation: {automation} min");
        println!("{date} foreground: {foreground} min");
    }
    Ok(())
}

/// `kynoptic watchdog`：托盘看门狗（给计划任务每分钟调一次，`--once` 单次检查）。
/// 探活 = 打开托盘的命名互斥体；托盘不在且非用户主动退出（无旗标）才拉起。
/// 常驻模式（默认）每 15s 检查一轮；被杀/崩溃 -> 重新拉起 --minimized。
/// P1 修复：除互斥体探活外还检查托盘心跳文件——进程活着但采集主循环挂死时
/// 互斥体不释放，旧逻辑永远不会重启（4-8 小时无数据空洞的根因）。心跳缺失
/// 或距今超过 180 秒且进程存在 -> kill 托盘，下一轮自动重新拉起。
///
/// 共享文件路径契约（见 crates/tray/src/paths.rs，两边必须同步修改）：
///   退出旗标 = KYNOPTIC_EXIT_FLAG env > exe 同目录 tray-exit.flag
///   心跳     = exe 同目录 kynoptic-heartbeat（RFC3339 时间戳，tray 每 30s touch）
///   看门狗日志 = exe 同目录 watchdog.log
const EXIT_FLAG_FILE: &str = "tray-exit.flag";
const HEARTBEAT_FILE: &str = "kynoptic-heartbeat";
const EXIT_FLAG_ENV: &str = "KYNOPTIC_EXIT_FLAG";
const WATCHDOG_LOG_FILE: &str = "watchdog.log";
/// 心跳最大年龄：tray 每 30s touch，180s ≈ 6 个周期未更新即判挂死
const HEARTBEAT_MAX_AGE_SECS: i64 = 180;
/// 看门狗状态文件（exe 同目录，记录连续拉起失败计数与退避窗口）
const WATCHDOG_STATE_FILE: &str = "watchdog-state.json";
/// watchdog.log 轮转后缀（覆盖式改名 watchdog.log.old）
const WATCHDOG_LOG_OLD_SUFFIX: &str = ".old";
/// watchdog.log 轮转阈值：1MB
const WATCHDOG_LOG_MAX_BYTES: u64 = 1024 * 1024;
/// 拉起观察窗：拉起后 90 秒内心跳未被刷新即记一次失败
const SPAWN_GRACE_SECS: i64 = 90;
/// 连续失败达到该次数后进入指数退避
const FAILURE_THRESHOLD: u32 = 3;
/// 指数退避档位：2min → 8min → 30min（封顶）
const BACKOFF_STEPS_SECS: [i64; 3] = [120, 480, 1800];
/// 睡眠唤醒守卫：心跳超龄后先等 40s 复查，仍超龄才 kill
const STALE_RECHECK_WAIT_SECS: u64 = 40;

fn exe_dir() -> Option<PathBuf> {
    std::env::current_exe().ok().and_then(|e| e.parent().map(|d| d.to_path_buf()))
}

/// 退出旗标路径：env 覆盖 > exe 同目录（与 tray 的 paths::resolve_exit_flag 同规则）
fn exit_flag_path() -> PathBuf {
    if let Ok(p) = std::env::var(EXIT_FLAG_ENV) {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    exe_dir()
        .map(|d| d.join(EXIT_FLAG_FILE))
        .unwrap_or_else(|| PathBuf::from(EXIT_FLAG_FILE))
}

/// 心跳文件路径：exe 同目录（与 tray 的 paths::resolve_heartbeat 同规则）
fn heartbeat_path() -> PathBuf {
    exe_dir()
        .map(|d| d.join(HEARTBEAT_FILE))
        .unwrap_or_else(|| PathBuf::from(HEARTBEAT_FILE))
}

/// 心跳新鲜度判定：内容应为 RFC3339 时间戳，返回距今秒数。
/// 缺失/解析失败返回 None（调用方按"过期"处理）；时钟回拨按 0 处理。
fn heartbeat_age_secs(now: chrono::DateTime<Utc>, content: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(content.trim())
        .ok()
        .map(|t| (now - t.with_timezone(&Utc)).num_seconds().max(0))
}

/// 心跳是否过期（缺失/不可解析/超龄都算过期）。
fn heartbeat_stale(now: chrono::DateTime<Utc>, content: Option<&str>) -> bool {
    match content {
        Some(c) => match heartbeat_age_secs(now, c) {
            Some(age) => age > HEARTBEAT_MAX_AGE_SECS,
            None => true,
        },
        None => true,
    }
}

/// 追加一行看门狗日志到 exe 同目录 watchdog.log（尽力而为，失败忽略）。
/// 写入前做大小轮转（P1）：超过 1MB 时改名为 watchdog.log.old（覆盖式），
/// 再建新文件——日志永不无限增长，也保留最近一份历史。
fn watchdog_log(msg: &str) {
    let line = format!("[{}] {}\n", Utc::now().to_rfc3339(), msg);
    eprint!("watchdog: {line}");
    if let Some(dir) = exe_dir() {
        let log_path = dir.join(WATCHDOG_LOG_FILE);
        rotate_log_if_needed(&log_path);
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
        {
            let _ = f.write_all(line.as_bytes());
        }
    }
}

/// 轮转判定（纯函数，单测覆盖）：当前大小超过阈值即需要轮转。
fn needs_log_rotation(size: u64) -> bool {
    size > WATCHDOG_LOG_MAX_BYTES
}

/// 轮转目标路径（纯函数）：watchdog.log → watchdog.log.old
fn rotated_log_path(log_path: &Path) -> PathBuf {
    let mut name = log_path
        .file_name()
        .map(|s| s.to_os_string())
        .unwrap_or_default();
    name.push(WATCHDOG_LOG_OLD_SUFFIX);
    log_path.with_file_name(name)
}

/// 大小超阈值时执行轮转（改名覆盖 .old，失败忽略——日志是尽力而为语义）。
fn rotate_log_if_needed(log_path: &Path) {
    let size = std::fs::metadata(log_path).map(|m| m.len()).unwrap_or(0);
    if needs_log_rotation(size) {
        let _ = std::fs::rename(log_path, rotated_log_path(log_path));
    }
}

// ─── 看门狗状态机（退避/失败计数）──—

/// 持久化状态（exe 同目录 watchdog-state.json）：跨看门狗进程重启保留
/// 失败计数与退避窗口，防止"杀看门狗再启"绕过熔断。
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
struct WatchdogState {
    /// 连续拉起失败次数（心跳恢复/拉起成功即清零）
    #[serde(default)]
    consecutive_failures: u32,
    /// 最近一次拉起的 unix 秒（0 = 无进行中的拉起）
    #[serde(default)]
    last_spawn_epoch: i64,
    /// 拉起那一刻心跳文件的 mtime（判定拉起后心跳是否被刷新过）
    #[serde(default)]
    heartbeat_at_spawn_epoch: i64,
    /// 熔断退避截止时刻的 unix 秒（0 = 不在退避中）
    #[serde(default)]
    backoff_until_epoch: i64,
}

fn watchdog_state_path() -> PathBuf {
    exe_dir()
        .map(|d| d.join(WATCHDOG_STATE_FILE))
        .unwrap_or_else(|| PathBuf::from(WATCHDOG_STATE_FILE))
}

/// 读状态；文件缺失/损坏一律回退缺省（计数丢失可接受，不能因此拒绝工作）。
fn load_watchdog_state() -> WatchdogState {
    std::fs::read_to_string(watchdog_state_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_watchdog_state(st: &WatchdogState) {
    if let Ok(json) = serde_json::to_string(st) {
        let _ = std::fs::write(watchdog_state_path(), json);
    }
}

/// 心跳文件的 mtime（unix 秒；缺失/不可得返回 0）。用 mtime 而非内容时间戳，
/// 避免系统睡眠导致内容时钟与真实经过时间脱节。
fn heartbeat_mtime_epoch() -> i64 {
    std::fs::metadata(heartbeat_path())
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 上一次拉起的结局判定（纯函数，单测覆盖）。
enum SpawnOutcome {
    /// 观察窗（90s）未到，暂不下结论
    Pending,
    /// 心跳在拉起后被刷新过（托盘确实跑起来过）
    Recovered,
    /// 观察窗已过且心跳从未刷新（典型：托盘启动即崩）——记一次失败
    Failed,
}

fn judge_spawn_outcome(
    last_spawn_epoch: i64,
    now_epoch: i64,
    hb_mtime_epoch: i64,
    hb_at_spawn_epoch: i64,
) -> SpawnOutcome {
    if last_spawn_epoch == 0 {
        return SpawnOutcome::Recovered; // 无进行中的拉起
    }
    if now_epoch - last_spawn_epoch < SPAWN_GRACE_SECS {
        return SpawnOutcome::Pending;
    }
    if hb_mtime_epoch > hb_at_spawn_epoch {
        SpawnOutcome::Recovered
    } else {
        SpawnOutcome::Failed
    }
}

/// 指数退避时长（纯函数）：失败次数未达阈值不退避（0）；达到后按档位
/// 2min → 8min → 30min 封顶。
fn backoff_delay_secs(consecutive_failures: u32) -> i64 {
    if consecutive_failures < FAILURE_THRESHOLD {
        return 0;
    }
    let idx = ((consecutive_failures - FAILURE_THRESHOLD) as usize)
        .min(BACKOFF_STEPS_SECS.len() - 1);
    BACKOFF_STEPS_SECS[idx]
}

/// 是否允许拉起（纯函数）：不在退避中，或退避窗已过。
enum SpawnDecision {
    Spawn,
    /// 熔断中，剩余秒数
    SkipBackoff(i64),
}

fn spawn_decision(
    consecutive_failures: u32,
    backoff_until_epoch: i64,
    now_epoch: i64,
) -> SpawnDecision {
    if backoff_delay_secs(consecutive_failures) == 0 || now_epoch >= backoff_until_epoch {
        SpawnDecision::Spawn
    } else {
        SpawnDecision::SkipBackoff(backoff_until_epoch - now_epoch)
    }
}

/// 睡眠唤醒守卫的复查判定（纯函数）：首次发现超龄后等待
/// STALE_RECHECK_WAIT_SECS 再复查。复查时年龄回落（心跳被重新 touch）或
/// 已回到阈值内 = 刚从睡眠唤醒的假象，放行；年龄继续增长且仍超龄才 kill。
fn recheck_should_kill(age_first_secs: i64, age_recheck_secs: i64) -> bool {
    age_recheck_secs > age_first_secs && age_recheck_secs > HEARTBEAT_MAX_AGE_SECS
}

/// kill 托盘进程（心跳挂死时）。返回是否至少执行了一次 taskkill。
#[cfg(target_os = "windows")]
fn kill_tray() -> bool {
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let out = Command::new("taskkill")
        .args(["/F", "/IM", "kynoptic-tray.exe", "/T"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .status();
    out.is_ok()
}

fn cmd_watchdog(args: &[String]) -> Result<()> {
    let mut once = false;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            // 历史兼容参数：旗标/心跳路径已统一为 exe 同目录（+env 覆盖），
            // --db 不再参与旗标定位，保留仅为不破坏计划任务里的旧命令行。
            "--db" => {
                it.next()
                    .ok_or_else(|| Error::InvalidData("--db 需要路径".into()))?;
            }
            "--once" => once = true,
            other => return Err(Error::InvalidData(format!("watchdog 未知参数: {other}"))),
        }
    }

    #[cfg(target_os = "windows")]
    {
        use windows_sys::Win32::System::Threading::OpenMutexW;
        const SYNCHRONIZE: u32 = 0x0010_0000;
        use std::os::windows::process::CommandExt;
        let name: Vec<u16> = r"Local\KynopticTrayMutex"
            .encode_utf16()
            .chain([0])
            .collect();
        let exit_flag = exit_flag_path();
        let mut state = load_watchdog_state();
        loop {
            let now_epoch = Utc::now().timestamp();
            let running = unsafe {
                let h = OpenMutexW(SYNCHRONIZE, 0, name.as_ptr());
                if !h.is_null() {
                    windows_sys::Win32::Foundation::CloseHandle(h);
                    true
                } else {
                    false
                }
            };
            if running {
                // 进程活着：检查心跳。缺失或超龄（默认 180s）说明采集主循环
                // 可能挂死——但先做睡眠唤醒守卫（P1：系统睡眠期间心跳自然
                // 超龄，直接 kill 属误杀），40s 复查仍超龄才 kill。
                let hb_read = std::fs::read_to_string(heartbeat_path())
                    .map(|s| s.trim().to_string())
                    .ok();
                let hb_age_first = hb_read
                    .as_deref()
                    .and_then(|c| heartbeat_age_secs(Utc::now(), c));
                if heartbeat_stale(Utc::now(), hb_read.as_deref()) {
                    // 睡眠唤醒守卫：等待 40s 后复查（纯函数 recheck_should_kill）
                    watchdog_log(&format!(
                        "心跳缺失或超龄(>{HEARTBEAT_MAX_AGE_SECS}s), 疑似睡眠唤醒/挂死, {STALE_RECHECK_WAIT_SECS}s 后复查"
                    ));
                    std::thread::sleep(std::time::Duration::from_secs(STALE_RECHECK_WAIT_SECS));
                    let still_running = unsafe {
                        let h = OpenMutexW(SYNCHRONIZE, 0, name.as_ptr());
                        if !h.is_null() {
                            windows_sys::Win32::Foundation::CloseHandle(h);
                            true
                        } else {
                            false
                        }
                    };
                    if still_running {
                        let hb_recheck = std::fs::read_to_string(heartbeat_path())
                            .map(|s| s.trim().to_string())
                            .ok();
                        let hb_age_recheck = hb_recheck
                            .as_deref()
                            .and_then(|c| heartbeat_age_secs(Utc::now(), c))
                            // 复查时心跳文件消失按"更糟"处理
                            .unwrap_or(i64::MAX);
                        let age_first = hb_age_first.unwrap_or(i64::MAX);
                        if recheck_should_kill(age_first, hb_age_recheck) {
                            watchdog_log(&format!(
                                "复查仍超龄(首次 {age_first}s → 复查 {hb_age_recheck}s), 判定采集挂死, kill kynoptic-tray 以重启"
                            ));
                            kill_tray();
                        } else {
                            watchdog_log(&format!(
                                "复查时心跳已刷新(首次 {age_first}s → 复查 {hb_age_recheck}s), 放行(睡眠唤醒假象)"
                            ));
                        }
                    } else {
                        watchdog_log("复查期间托盘已退出, 交由下一轮拉起路径处理");
                    }
                } else {
                    // 心跳健康：熔断计数清零（P1：手动启动托盘成功即恢复拉起）
                    if state.consecutive_failures != 0
                        || state.last_spawn_epoch != 0
                        || state.backoff_until_epoch != 0
                    {
                        state = WatchdogState::default();
                        save_watchdog_state(&state);
                        watchdog_log("心跳恢复正常, 连续失败计数与退避已清零");
                    }
                }
            } else if !exit_flag.exists() {
                // 先结算上一轮拉起的结局（观察窗 90s）
                match judge_spawn_outcome(
                    state.last_spawn_epoch,
                    now_epoch,
                    heartbeat_mtime_epoch(),
                    state.heartbeat_at_spawn_epoch,
                ) {
                    SpawnOutcome::Pending => {}
                    SpawnOutcome::Recovered => {
                        // 心跳被刷新过 = 上轮拉起成功运行过；结束观察窗
                        state.last_spawn_epoch = 0;
                    }
                    SpawnOutcome::Failed => {
                        state.consecutive_failures += 1;
                        state.last_spawn_epoch = 0;
                        let delay = backoff_delay_secs(state.consecutive_failures);
                        state.backoff_until_epoch = now_epoch + delay;
                        save_watchdog_state(&state);
                        watchdog_log(&format!(
                            "拉起后 {SPAWN_GRACE_SECS}s 内心跳未刷新, 连续失败 #{}（{}）",
                            state.consecutive_failures,
                            if delay > 0 {
                                format!("进入指数退避 {delay}s")
                            } else {
                                "未达退避阈值".to_string()
                            }
                        ));
                    }
                }
                match spawn_decision(
                    state.consecutive_failures,
                    state.backoff_until_epoch,
                    now_epoch,
                ) {
                    SpawnDecision::SkipBackoff(remaining) => {
                        watchdog_log(&format!(
                            "连续失败 {} 次, 熔断退避中, 约 {}s 后重试拉起",
                            state.consecutive_failures, remaining
                        ));
                    }
                    SpawnDecision::Spawn => {
                        if let Ok(exe) = std::env::current_exe() {
                            if let Some(dir) = exe.parent() {
                                let tray = dir.join("kynoptic-tray.exe");
                                if tray.exists() {
                                    use std::process::{Command, Stdio};
                                    const DETACHED_PROCESS: u32 = 0x0000_0008;
                                    match Command::new(&tray)
                                        .arg("--minimized")
                                        .stdin(Stdio::null())
                                        .stdout(Stdio::null())
                                        .stderr(Stdio::null())
                                        .creation_flags(DETACHED_PROCESS)
                                        .spawn()
                                    {
                                        Ok(_) => {
                                            state.last_spawn_epoch = now_epoch;
                                            state.heartbeat_at_spawn_epoch =
                                                heartbeat_mtime_epoch();
                                            save_watchdog_state(&state);
                                        }
                                        Err(e) => {
                                            eprintln!("watchdog: 拉起托盘失败: {e}");
                                        }
                                    }
                                    watchdog_log("托盘不在且非用户退出,已拉起");
                                }
                            }
                        }
                    }
                }
            }
            if once {
                return Ok(());
            }
            std::thread::sleep(std::time::Duration::from_secs(15));
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = once;
        Ok(())
    }
}

/// CSV 公式注入中和（审查 P1：窗口标题是网页等外部可控输入，以 = + - @
/// 或制表符开头的单元格被 Excel 打开会当公式执行）。前缀单引号使其降级
/// 为纯文本。
fn csv_cell(v: &str) -> String {
    if v.starts_with(['=', '+', '-', '@', '\t', '\r', '\n']) {
        format!("'{v}")
    } else {
        v.to_string()
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let sub = args.first().map(|s| s.as_str()).unwrap_or("help");
    let result: Result<()> = match sub {
        "collect" => cmd_collect(&args[1..]),
        "stats" => cmd_stats(&args[1..]),
        "export" => cmd_export(&args[1..]),
        "report" => cmd_report(&args[1..]),
        "db" => cmd_db(&args[1..]),
        "analyze" => cmd_analyze(&args[1..]),
        "ghost" => cmd_ghost(),
        "autostart" => cmd_autostart(&args[1..]),
        "migrate" => cmd_migrate(&args[1..]),
        "now" => cmd_now(&args[1..]),
        "query" => cmd_query(&args[1..]),
        "mcp" => cmd_mcp(),
        "probe" => cmd_probe(&args[1..]),
        "dashboard" => dashboard::cmd_dashboard(&args[1..]),
        "update" => update::cmd_update(&args[1..]),
        "watchdog" => cmd_watchdog(&args[1..]),
        "presence" => cmd_presence(&args[1..]),
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

#[cfg(test)]
mod tests {
    use super::*;

    // === parse_when ===

    #[test]
    fn parse_when_pure_date_respects_upper_bound() {
        let lower = parse_when("2026-09-09", false).unwrap();
        let upper = parse_when("2026-09-09", true).unwrap();
        assert!(
            lower < upper,
            "同一天的上界应为其次日零点: {lower} < {upper}"
        );
        // 上界恰为下界 + 1 天
        let lu = chrono::DateTime::parse_from_rfc3339(&lower).unwrap();
        let uu = chrono::DateTime::parse_from_rfc3339(&upper).unwrap();
        assert_eq!((uu - lu).num_hours(), 24);
    }

    #[test]
    fn parse_when_yesterday_shifts_with_bound() {
        let lower = parse_when("yesterday", false).unwrap();
        let upper = parse_when("yesterday", true).unwrap();
        assert!(lower < upper);
        // --to yesterday = 今日起点
        let (today_start, _) = queries::today_range();
        assert_eq!(upper, today_start);
    }

    #[test]
    fn parse_when_rfc3339_passthrough_and_naive_datetime() {
        let t = parse_when("2026-09-09T12:30:00+08:00", false).unwrap();
        assert_eq!(t, "2026-09-09T12:30:00+08:00");
        // 无时区的钟表时间按本地时区解释（审查 P2：按 UTC 补偏移会错 8 小时）
        let naive = parse_when("2026-09-09T12:30", false).unwrap();
        assert!(naive.starts_with("2026-09-09T12:30:00"), "{naive}");
        let naive_s = parse_when("2026-09-09T12:30:45", true).unwrap();
        assert!(naive_s.starts_with("2026-09-09T12:30:45"), "{naive_s}");
    }

    #[test]
    fn parse_when_rejects_garbage() {
        assert!(parse_when("not-a-time", false).is_err());
        assert!(parse_when("", false).is_err());
    }

    // === parse_bucket ===

    #[test]
    fn parse_bucket_maps_bucket_ids_to_event_types() {
        assert_eq!(parse_bucket("activity/keys").unwrap(), "keyboard");
        assert_eq!(parse_bucket("activity/mouse").unwrap(), "mouse");
        assert_eq!(parse_bucket("app/window").unwrap(), "window");
        assert_eq!(parse_bucket("system/anything").unwrap(), "system");
        assert_eq!(parse_bucket("keyboard").unwrap(), "keyboard");
    }

    #[test]
    fn parse_bucket_rejects_unknown() {
        assert!(parse_bucket("pet/mood").is_err());
        assert!(parse_bucket("").is_err());
    }

    // === parse_query_args ===

    #[test]
    fn query_args_defaults_and_clamping() {
        let q = parse_query_args(&[]).unwrap();
        assert!(!q.json);
        assert_eq!(q.limit, 50);
        assert!(q.from.is_none() && q.to.is_none() && q.bucket.is_none());

        let a: Vec<String> = ["--limit", "9999", "--json", "--bucket", "keyboard"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let q = parse_query_args(&a).unwrap();
        assert_eq!(q.limit, 1000, "limit 钳到 1000");
        assert!(q.json);
        assert_eq!(q.bucket.as_deref(), Some("keyboard"));
    }

    #[test]
    fn query_args_rejects_unknown_flag() {
        let a: Vec<String> = vec!["--gpu".to_string()];
        assert!(parse_query_args(&a).is_err());
    }

    // === 输出整形 ===

    #[test]
    fn truncate_col_caps_long_text() {
        assert_eq!(truncate_col("short", 16), "short");
        let long = "x".repeat(100);
        let t = truncate_col(&long, 16);
        assert_eq!(t.chars().count(), 16);
        assert!(t.ends_with('…'));
    }

    // === watchdog 心跳与共享路径 ===

    #[test]
    fn heartbeat_age_parses_rfc3339_and_clamps_clock_skew() {
        let now = Utc::now();
        let fresh = now - Duration::seconds(30);
        let age = heartbeat_age_secs(now, &fresh.to_rfc3339()).unwrap();
        assert!(
            (29..=30).contains(&age),
            "30s 前的心跳 ≈ 30s 年龄（RFC3339 截断亚秒可差 1）: {age}"
        );
        // 时钟回拨（内容在未来）按 0 处理,不判负
        let future = now + Duration::seconds(120);
        assert_eq!(heartbeat_age_secs(now, &future.to_rfc3339()), Some(0));
        // 首尾空白可容忍;坏内容 = None
        assert_eq!(heartbeat_age_secs(now, "  not-a-time \n"), None);
        assert_eq!(heartbeat_age_secs(now, ""), None);
    }

    #[test]
    fn heartbeat_stale_missing_or_old_or_garbage() {
        let now = Utc::now();
        // 缺失 = 过期
        assert!(heartbeat_stale(now, None));
        // 垃圾内容 = 过期
        assert!(heartbeat_stale(now, Some("garbage")));
        // 新鲜（阈值内）不过期
        let recent = (now - Duration::seconds(HEARTBEAT_MAX_AGE_SECS - 1)).to_rfc3339();
        assert!(!heartbeat_stale(now, Some(&recent)));
        // 恰好等于阈值不算过期,超过 1 秒过期
        let exact = (now - Duration::seconds(HEARTBEAT_MAX_AGE_SECS)).to_rfc3339();
        assert!(!heartbeat_stale(now, Some(&exact)));
        let old = (now - Duration::seconds(HEARTBEAT_MAX_AGE_SECS + 1)).to_rfc3339();
        assert!(heartbeat_stale(now, Some(&old)));
    }

    // env 是进程全局的:两个用到 exit_flag_path 的用例共用一把锁串行化
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn shared_paths_match_tray_contract_filenames() {
        let _g = ENV_LOCK.lock().unwrap();
        // 与 crates/tray/src/paths.rs 的契约:文件名必须一致
        assert_eq!(EXIT_FLAG_FILE, "tray-exit.flag");
        assert_eq!(HEARTBEAT_FILE, "kynoptic-heartbeat");
        assert_eq!(EXIT_FLAG_ENV, "KYNOPTIC_EXIT_FLAG");
        // 解析规则:exe 同目录(文件名锚定)
        assert_eq!(exit_flag_path().file_name().unwrap(), "tray-exit.flag");
        assert_eq!(heartbeat_path().file_name().unwrap(), "kynoptic-heartbeat");
    }

    #[test]
    fn exit_flag_env_override_matches_tray() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::set_var(EXIT_FLAG_ENV, r"C:\tmp\ctl-flag");
        let p = exit_flag_path();
        std::env::remove_var(EXIT_FLAG_ENV);
        assert_eq!(p, PathBuf::from(r"C:\tmp\ctl-flag"));
    }

    // === presence ===

    #[test]
    fn presence_args_default_one_day_and_validate() {
        assert_eq!(parse_presence_args(&[]).unwrap(), 1);
        let a: Vec<String> = ["--days", "7"].iter().map(|s| s.to_string()).collect();
        assert_eq!(parse_presence_args(&a).unwrap(), 7);
        for bad in [
            vec!["--days".to_string(), "abc".to_string()],
            vec!["--days".to_string(), "0".to_string()],
            vec!["--days".to_string(), "-1".to_string()],
            vec!["--days".to_string()],
            vec!["--gpu".to_string()],
        ] {
            assert!(parse_presence_args(&bad).is_err(), "{bad:?} 应报错");
        }
    }

    #[test]
    fn bridge_count_bridges_small_gaps_only() {
        assert_eq!(bridge_count(&[], 2), 0);
        assert_eq!(bridge_count(&[10], 2), 1);
        // 10,11,12 连续;12->15 与 15->18 间隙均为 3,各按 cap(bridge+1)=3 补步长
        assert_eq!(bridge_count(&[10, 11, 12, 15, 18], 2), 9);
        // 间隙 4（缺 3 分钟）同样按 cap=3 补: 1 + 3 = 4
        assert_eq!(bridge_count(&[10, 14], 2), 4);
        // 连续分钟逐 1 累计
        assert_eq!(bridge_count(&[10, 11, 12, 13], 2), 4);
    }

    /// 建最小 events 表并插入测试数据（本地日:今天）。
    fn setup_presence_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE events (id INTEGER PRIMARY KEY, timestamp TEXT, event_type TEXT, \
             event_action TEXT, event_data TEXT, app_name TEXT, window_title TEXT, session_id INTEGER);",
        )
        .unwrap();
        conn
    }

    fn insert_event(conn: &Connection, ts: &str, etype: &str, action: &str, data: &str) {
        conn.execute(
            "INSERT INTO events (timestamp, event_type, event_action, event_data) \
             VALUES (?1, ?2, ?3, ?4)",
            params![ts, etype, action, data],
        )
        .unwrap();
    }

    #[test]
    fn presence_metrics_counts_human_automation_foreground() {
        let conn = setup_presence_db();
        // 10:00 人工输入（keys=10, injected=0）-> 在场
        insert_event(&conn, "2026-09-13T10:00:30+08:00", "keyboard", "input_agg", r#"{"keys":10}"#);
        // 10:07 全注入 -> 自动化
        insert_event(&conn, "2026-09-13T10:07:00+08:00", "keyboard", "input_agg", r#"{"keys":5,"injected_keys":5}"#);
        // 混合分钟（human=8>0）-> 在场,不算自动化
        insert_event(&conn, "2026-09-13T10:08:00+08:00", "keyboard", "input_agg", r#"{"keys":10,"injected_keys":2}"#);
        // 窗口切换:10:00 -> 11:00 = 60 分钟前台
        insert_event(&conn, "2026-09-13T10:00:00+08:00", "window", "switch", "");
        insert_event(&conn, "2026-09-13T11:00:00+08:00", "window", "switch", "");
        let (start, end) = queries::local_day_range("2026-09-13").unwrap();
        let (p, a, f) = presence_metrics(&conn, &start, &end, 2).unwrap();
        assert_eq!(a, 1, "注入分钟数 = 1");
        // 大间隙按 dash 同款公式只补 bridge+1 步长（cap 后为 3）: 1 + 3 = 4
        assert_eq!(p, 4, "两个在场分钟 + 间隙按 bridge 上限补步长（与 dash 口径一致）");
        assert_eq!(f, 60, "前台 = 一次切换间隔 60 分钟");
    }

    #[test]
    fn presence_metrics_bridges_adjacent_human_minutes() {
        let conn = setup_presence_db();
        for m in ["10:00", "10:01", "10:04", "10:05"] {
            insert_event(
                &conn,
                &format!("2026-09-13T{m}:30+08:00"),
                "keyboard",
                "input_agg",
                r#"{"keys":3}"#,
            );
        }
        let (start, end) = queries::local_day_range("2026-09-13").unwrap();
        let (p, a, _) = presence_metrics(&conn, &start, &end, 2).unwrap();
        assert_eq!(a, 0);
        assert_eq!(p, 6, "10:00-10:01 + 桥接 10:02-10:03 + 10:04-10:05 = 6 分钟");
    }

    #[test]
    fn presence_metrics_empty_day_is_zero() {
        let conn = setup_presence_db();
        let (start, end) = queries::local_day_range("2026-09-13").unwrap();
        assert_eq!(presence_metrics(&conn, &start, &end, 2).unwrap(), (0, 0, 0));
    }

    // === watchdog 指数退避状态机 ===

    #[test]
    fn backoff_delay_steps_and_cap() {
        assert_eq!(backoff_delay_secs(0), 0, "未达阈值不退避");
        assert_eq!(backoff_delay_secs(1), 0);
        assert_eq!(backoff_delay_secs(2), 0);
        assert_eq!(backoff_delay_secs(3), 120, "3 连败 → 2min");
        assert_eq!(backoff_delay_secs(4), 480, "4 连败 → 8min");
        assert_eq!(backoff_delay_secs(5), 1800, "5 连败 → 30min");
        assert_eq!(backoff_delay_secs(6), 1800, "封顶 30min");
        assert_eq!(backoff_delay_secs(99), 1800, "封顶 30min");
    }

    #[test]
    fn judge_spawn_outcome_pending_recovered_failed() {
        let hb_at_spawn = 1000;
        // 无进行中的拉起 → Recovered（无观察窗）
        assert!(matches!(
            judge_spawn_outcome(0, 2000, 0, 0),
            SpawnOutcome::Recovered
        ));
        // 观察窗未到 → Pending（即使心跳没刷新也不下结论）
        assert!(matches!(
            judge_spawn_outcome(2000, 2000 + SPAWN_GRACE_SECS - 1, 1000, hb_at_spawn),
            SpawnOutcome::Pending
        ));
        // 观察窗已过 + 心跳被刷新过 → Recovered（托盘跑起来过）
        assert!(matches!(
            judge_spawn_outcome(2000, 2000 + SPAWN_GRACE_SECS, 1001, hb_at_spawn),
            SpawnOutcome::Recovered
        ));
        // 观察窗已过 + 心跳从未刷新（mtime 未变/文件缺失）→ Failed
        assert!(matches!(
            judge_spawn_outcome(2000, 2000 + SPAWN_GRACE_SECS, hb_at_spawn, hb_at_spawn),
            SpawnOutcome::Failed
        ));
        assert!(matches!(
            judge_spawn_outcome(2000, 3000, 0, hb_at_spawn),
            SpawnOutcome::Failed
        ));
    }

    #[test]
    fn spawn_decision_skips_only_during_backoff_window() {
        // 未达阈值：任何时候都允许拉起
        assert!(matches!(
            spawn_decision(2, 5000, 1000),
            SpawnDecision::Spawn
        ));
        // 达到阈值、窗口未过：跳过并给出剩余秒数
        match spawn_decision(3, 5000, 3000) {
            SpawnDecision::SkipBackoff(remaining) => assert_eq!(remaining, 2000),
            _ => panic!("退避窗口内应跳过拉起"),
        }
        // 退避中但 now==until：放行
        assert!(matches!(
            spawn_decision(3, 5000, 5000),
            SpawnDecision::Spawn
        ));
        // 窗口已过：放行
        assert!(matches!(
            spawn_decision(5, 5000, 5001),
            SpawnDecision::Spawn
        ));
    }

    #[test]
    fn watchdog_state_serializes_roundtrip_and_defaults_on_garbage() {
        let st = WatchdogState {
            consecutive_failures: 4,
            last_spawn_epoch: 123,
            heartbeat_at_spawn_epoch: 100,
            backoff_until_epoch: 9999,
        };
        let json = serde_json::to_string(&st).unwrap();
        let back: WatchdogState = serde_json::from_str(&json).unwrap();
        assert_eq!(back.consecutive_failures, 4);
        assert_eq!(back.backoff_until_epoch, 9999);
        // 损坏/缺字段：serde default 兜底 + 顶层回退缺省
        let partial: WatchdogState =
            serde_json::from_str(r#"{"consecutive_failures":2}"#).unwrap();
        assert_eq!(partial.consecutive_failures, 2);
        assert_eq!(partial.last_spawn_epoch, 0);
        assert!(serde_json::from_str::<WatchdogState>("garbage").is_err());
    }

    // === watchdog 睡眠唤醒守卫 ===

    #[test]
    fn recheck_guard_spares_refreshed_heartbeat() {
        // 复查时年龄回落（心跳被重新 touch）→ 放行
        assert!(!recheck_should_kill(200, 35));
        // 复查时年龄回到阈值内 → 放行
        assert!(!recheck_should_kill(200, HEARTBEAT_MAX_AGE_SECS));
        // 复查时年龄仍在增长（首次 + 等待窗）且超龄 → kill
        assert!(recheck_should_kill(200, 240));
        // 复查时心跳文件消失（i64::MAX）→ kill
        assert!(recheck_should_kill(200, i64::MAX));
    }

    // === watchdog.log 轮转 ===

    #[test]
    fn log_rotation_threshold_and_path() {
        assert!(!needs_log_rotation(0));
        assert!(!needs_log_rotation(WATCHDOG_LOG_MAX_BYTES));
        assert!(needs_log_rotation(WATCHDOG_LOG_MAX_BYTES + 1));
        let p = Path::new(r"C:\apps\watchdog.log");
        assert_eq!(
            rotated_log_path(p),
            PathBuf::from(r"C:\apps\watchdog.log.old")
        );
        assert_eq!(rotated_log_path(Path::new("watchdog.log")), PathBuf::from("watchdog.log.old"));
    }

    // === export 脱敏与缺省目录 ===

    #[test]
    fn sanitize_window_title_strips_query_only_for_http_fragments() {
        // 含 http 的片段：剥掉 ? 及之后
        assert_eq!(
            sanitize_window_title("登录 - https://example.com/login?token=abc&session=x"),
            "登录 - https://example.com/login"
        );
        // 无 ? 的 URL 原样保留
        assert_eq!(
            sanitize_window_title("https://example.com/home"),
            "https://example.com/home"
        );
        // 不含 http 的片段即使有 ? 也不动
        assert_eq!(
            sanitize_window_title("文件?草稿.txt - 记事本"),
            "文件?草稿.txt - 记事本"
        );
        // 混合片段：只处理含 http 的
        assert_eq!(
            sanitize_window_title("报表 v2 https://a.io/x?y=1 done"),
            "报表 v2 https://a.io/x done"
        );
    }

    #[test]
    fn default_export_dir_is_db_sibling_exports() {
        assert_eq!(
            default_export_dir(Path::new("C:/data/kynoptic.db")),
            PathBuf::from("C:/data/exports")
        );
        assert_eq!(
            default_export_dir(Path::new("kynoptic.db")),
            PathBuf::from("exports")
        );
    }

    // === report / analyze 输出整形 ===

    #[test]
    fn md_cell_escapes_pipe_and_newline() {
        assert_eq!(md_cell("IDE"), "IDE");
        assert_eq!(md_cell("a|b"), "a\\|b");
        assert_eq!(md_cell("line1\nline2"), "line1 line2");
        assert_eq!(md_cell("a\r\nb|c"), "a  b\\|c");
    }

    #[test]
    fn strip_c0_keeps_newline_drops_other_controls() {
        assert_eq!(strip_c0("plain"), "plain");
        assert_eq!(strip_c0("a\nb"), "a\nb");
        assert_eq!(strip_c0("a\u{1b}[31mb"), "a[31mb");
        assert_eq!(strip_c0("a\u{7}b\u{0}c"), "abc");
        assert_eq!(strip_c0("a\u{9}b"), "ab", "制表符属 C0,一并滤除");
    }

    #[test]
    fn csv_cell_neutralizes_formula_prefix() {
        assert_eq!(csv_cell("=cmd|' /C calc'!A0"), "'=cmd|' /C calc'!A0");
        assert_eq!(csv_cell("@SUM"), "'@SUM");
        assert_eq!(csv_cell("普通标题"), "普通标题");
    }
}
