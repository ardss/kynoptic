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
//!   now       — 当前机器状态（compact 表格 / --json）
//!   query     — 时间范围事件查询 (--from/--to/--bucket/--limit/--json)
//!   mcp       — 启动 MCP server（stdio，Claude Desktop: command "kynoptic" args ["mcp"]）

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
  now       [--json]                          Current machine status (compact)
  query     [--from T] [--to T] [--bucket B] [--limit N] [--json]
                                              Event query in time range
  mcp                                         Run MCP server over stdio
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
        "now" => cmd_now(&args[1..]),
        "query" => cmd_query(&args[1..]),
        "mcp" => cmd_mcp(),
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
        let naive = parse_when("2026-09-09T12:30", false).unwrap();
        assert_eq!(naive, "2026-09-09T12:30:00+00:00");
        let naive_s = parse_when("2026-09-09T12:30:45", true).unwrap();
        assert_eq!(naive_s, "2026-09-09T12:30:45+00:00");
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
}
