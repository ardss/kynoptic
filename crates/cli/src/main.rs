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
  export    [--days N] [--format csv|json|jsonl] [--out PATH] [--redact]
  report    [--date today|yesterday|YYYY-MM-DD] [--save PATH]   Generate Markdown report
  db        [stats|cleanup [N]|vacuum|checkpoint|recompute-agg]   DB maintenance
  analyze   [--date YYYY-MM-DD] [--days N]     Focus/fragment/anomaly report
  ghost                                       Close ghost sessions
  autostart [enable|disable|status]            Toggle auto-start
  migrate   [--legacy PATH] [--target PATH]   Migrate from legacy db
  now       [--json]                          Current machine status (compact)
  query     [--from T] [--to T] [--bucket B] [--limit N] [--json]
                                              Event query in time range
  mcp                                         Run MCP server over stdio
  skill install                               Sync bundled SKILL.md to AI client skill dirs
  --version / -V                              Print version
  probe     [--monitor ID] [--secs N] [--all] Live per-monitor hardware probe
  dashboard [--port N] [--db PATH]          Local-only read-only web dashboard
  update [--check]                          Self-update from GitHub releases (--check: report only)
  watchdog [--once]                         Ensure tray is alive (for Task Scheduler)
  presence  [--days N]                      Daily presence/automation/foreground summary

Global options (all subcommands unless noted):
  --db PATH   Explicit db path, overrides the default resolution. When absent, \
the resolved default path is printed to stderr as \"using db: <path>\". \
(dashboard keeps its own --db handling)
";

/// 全局 --db 覆盖（审查 P2 修复）：main 里从参数摘出后写入，resolve_db()
/// 优先使用。缺 --db 时所有子命令都会静默落到 resolve_db_path() 的默认推导，
/// 曾实测裸跑 `export` 解析到 target/release/data 下陈旧库、导出 0 行不报错。
static DB_OVERRIDE: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);

fn set_db_override(p: PathBuf) {
    if let Ok(mut g) = DB_OVERRIDE.lock() {
        *g = Some(p);
    }
}

/// 从参数中摘出 `--db <PATH>`（成对消费，允许多次出现取最后一次）。
/// dashboard 子命令自带 --db 解析，main 里对它跳过本函数。
fn extract_global_db(args: &mut Vec<String>) -> Result<()> {
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--db" {
            let val = args
                .get(i + 1)
                .cloned()
                .ok_or_else(|| Error::InvalidData("--db 需要路径".into()))?;
            args.drain(i..=i + 1);
            set_db_override(PathBuf::from(val));
            // 不 i += 1：继续检查同一位置（防 "--db --db x" 之类的畸形输入死循环也无所谓，drain 已消费）
        } else {
            i += 1;
        }
    }
    Ok(())
}

fn resolve_db() -> PathBuf {
    // 显式 --db 优先
    if let Some(p) = DB_OVERRIDE.lock().ok().and_then(|g| g.clone()) {
        return p;
    }
    // 复用 core 的统一路径解析逻辑，与主应用保持一致（exe 同级优先，cwd 兜底）。
    let p = kynoptic_core::db::resolve_db_path();
    // 静默变可见：走默认推导时把实际使用的库路径打到 stderr，防止"操作了
    // 陈旧库却以为在操作生产库"类误判（审查 P2 实测案例）。
    eprintln!("using db: {}", p.display());
    p
}

fn open_db(path: &Path) -> Result<Connection> {
    // 复用 core 的初始化逻辑（SCHEMA + 迁移 + 全套 PRAGMA），
    // 替代原只设 2 个 PRAGMA 的实现——避免 ctl 操作比应用旧的库时缺列。
    let conn = Connection::open(path)?;
    kynoptic_core::db::apply_pragmas(&conn)?;
    conn.execute_batch(kynoptic_core::db::SCHEMA)?;
    // Wave22 P1：不再吞迁移错误——带病运行会让 stats 静默报旧 schema 的数
    kynoptic_core::db::run_migrations(&conn)?;
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
                // Wave19：非法值报错而非静默回落（与 presence/analyze 同政策）
                days = args
                    .get(i)
                    .and_then(|s| s.parse().ok())
                    .ok_or_else(|| Error::InvalidData("--days 需要一个正整数".into()))?;
                if days < 1 {
                    return Err(Error::InvalidData("--days 需要 >=1".into()));
                }
            }
            other => {
                return Err(Error::InvalidData(format!(
                    "未知选项: {other}（支持 --date/--days）"
                )))
            }
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
        // 与 presence/analyze 同口径：按本地日历日输出满 N 天。daily_agg::recent
        // 的 LIMIT 查询只返回有聚合行的日，无行日历日（未开机整天等）曾被静默
        // 跳过，"last N days" 表头下实际少于 N 行。缺行日补零。
        let mut by_date = std::collections::BTreeMap::new();
        for row in daily_agg::recent(&conn, days) {
            by_date.insert(row.0.clone(), row);
        }
        for offset in (1 - days)..=0 {
            let date = queries::date_offset_str(offset);
            match by_date.get(&date) {
                Some(row) => println!(
                    "{:<12} {:>7} {:>7} {:>7} {:>7.1}",
                    row.0, row.1, row.2, row.3, row.4
                ),
                None => println!("{:<12} {:>7} {:>7} {:>7} {:>7.1}", date, 0, 0, 0, 0.0),
            }
        }
    }
    Ok(())
}

// === export ===
fn cmd_export(args: &[String]) -> Result<()> {
    let mut days: i64 = 7;
    let mut format = "csv".to_string();
    let mut out = String::new();
    let mut redact = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--days" => {
                i += 1;
                days = args
                    .get(i)
                    .and_then(|s| s.parse().ok())
                    .ok_or_else(|| Error::InvalidData("--days 需要一个正整数".into()))?;
                if days < 1 {
                    return Err(Error::InvalidData("--days 需要 >=1".into()));
                }
            }
            "--format" => {
                i += 1;
                format = args.get(i).cloned().unwrap_or_else(|| "csv".to_string());
            }
            "--out" => {
                i += 1;
                out = args.get(i).cloned().unwrap_or_default();
            }
            // --redact（opt-in）：剥离含 http 片段的 URL 查询串。默认输出
            // window_title 完整原文——本地数据完整优先，导出即备份原文。
            "--redact" => redact = true,
            // 历史兼容：旧 --raw 语义（保留原文）即现在的默认行为，接受但不做事
            "--raw" => {}
            other => {
                return Err(Error::InvalidData(format!(
                    "未知选项: {other}（支持 --days/--format/--out/--redact）"
                )))
            }
        }
        i += 1;
    }
    let db_path = resolve_db();
    // 极限注入审查：--days 9223372036854775807 曾在 chrono TimeDelta::days
    // panic（out of bounds）。钳到 20 万天（≈547 年，早于任何可能的数据，
    // 也在 DateTime 表示范围内）= 等效全量导出；days<1 已在解析处拒绝。
    let days = days.min(200_000);
    // 与面板/其余命令统一口径：导出窗口 = 含今天在内的 N 个本地日。
    // 旧实现取 (今日 - N) 本地日起点作 cutoff，实际覆盖 N+1 个日历日
    //（--days 1 连昨日整天一起导出多泄一天敏感明文），与 daily_agg
    // 「最近 days 天（含今天）」及 query/presence/stats 均不一致。
    let cutoff_date = queries::date_offset_str(-(days - 1));
    let cutoff = match queries::local_day_range(&cutoff_date) {
        Some((start, _)) => start,
        None => {
            // 本地日期换算失败时的兜底：退回滚动 UTC 瞬间（旧行为）
            (Utc::now() - Duration::try_days(days).unwrap_or(Duration::days(200_000))).to_rfc3339()
        }
    };
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

    // window_title 默认输出原文（本地数据完整优先）；--redact 显式开启才剥查询串
    let title_out = |t: &Option<String>| -> String {
        let t = t.clone().unwrap_or_default();
        if redact {
            sanitize_window_title(&t)
        } else {
            t
        }
    };

    match format.as_str() {
        "csv" => {
            use std::io::Write;
            let f = std::fs::File::create(&out_path)?;
            let mut buf = std::io::BufWriter::new(f);
            // P2：写 UTF-8 BOM（EF BB BF），Excel 双击打开才不会把 UTF-8 中文
            // 当 ANSI 读出乱码。仅 CSV——jsonl/json 是程序间交换格式,加 BOM
            // 反而破坏解析。（写在 csv::Writer 包装之前的裸 writer 上）
            buf.write_all(&UTF8_BOM)?;
            let mut w = csv::Writer::from_writer(buf);
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

/// UTF-8 BOM（CSV 导出专用，Excel 识别中文所必需）
const UTF8_BOM: [u8; 3] = [0xEF, 0xBB, 0xBF];

/// URL 查询串剥离（--redact 时启用）：仅对含 "http" 的 token 剥掉第一个 `?`
/// 及其后内容（查询串常带 token/session id 等敏感参数）。token 允许携带
/// 尾部空白（split_inclusive 的分隔符），截断后原样保留。
fn sanitize_url_query(token: &str) -> String {
    let word_end = token.find(char::is_whitespace).unwrap_or(token.len());
    let (word, tail) = token.split_at(word_end);
    // Wave24：大小写不敏感（HTTP:// 大写协议此前整段漏脱敏）
    if word.to_lowercase().contains("http") {
        match word.find('?') {
            Some(i) => format!("{}{}", &word[..i], tail),
            None => token.to_string(),
        }
    } else {
        token.to_string()
    }
}

/// window_title 脱敏（--redact opt-in）：P3 修复——旧实现
/// split_whitespace+join 会把换行/制表符压平成单空格，redact 版备份信息
/// 永久损失。现改为 `split_inclusive` 按空白切分且保留分隔符：只有含 "http"
/// 的连续非空白段（URL token）剥查询串，其余字符（含全部换行/制表符/多
/// 空格）原样保留。
fn sanitize_window_title(title: &str) -> String {
    title
        .split_inclusive(|c: char| c.is_whitespace())
        .map(sanitize_url_query)
        .collect()
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

/// report 的 --date 解析：空 = 今天；today/yesterday 别名（复用 parse_when，
/// 与 query --from/--to 同口径）；其余走 parse_date（YYYY-MM-DD）。
/// 纯逻辑抽出以便单测。
fn resolve_report_date(raw: &str) -> Result<String> {
    if raw.is_empty() {
        return Ok(queries::today_local_str());
    }
    if raw == "today" || raw == "yesterday" {
        // 与 parse_when 的 today/yesterday 分支同一时钟源（queries::date_offset_str，
        // 本地日界）。不截 parse_when 返回的 RFC3339 前 10 字符——那是 UTC 边界，
        // UTC+8 的本地凌晨会错位一天。
        Ok(queries::date_offset_str(if raw == "today" {
            0
        } else {
            -1
        }))
    } else {
        parse_date(raw)
    }
}

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
            other => {
                return Err(Error::InvalidData(format!(
                    "未知选项: {other}（report 支持 --date/--save）"
                )))
            }
        }
        i += 1;
    }
    let date = resolve_report_date(&date)?;
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
            // Wave24：兼容 `--days N` flag 写法（此前 positional-only，
            // 写 --days 会把 flag 本身当天数报误导性错误）
            let mut it = args[1..].iter();
            while let Some(a) = it.next() {
                match a.as_str() {
                    "--yes" => with_events = true,
                    "--days" => {
                        let v = it.next().ok_or_else(|| {
                            Error::InvalidData("cleanup: --days 需要一个天数".into())
                        })?;
                        days_arg =
                            Some(v.parse().map_err(|_| {
                                Error::InvalidData(format!("cleanup: 无效天数 {v:?}"))
                            })?);
                    }
                    v => {
                        days_arg =
                            Some(v.parse().map_err(|_| {
                                Error::InvalidData(format!("cleanup: 无效天数 {v:?}"))
                            })?);
                    }
                }
            }
            let days = days_arg.unwrap_or(90);
            // 铁律（e2e C1）：cleanup 0 不得删除任何东西。retention = 0 的语义是
            // "永不清理"，与 core 侧 DEFAULT_RETENTION_DAYS=0 及 Database::maintenance()
            // 的日常路径同源（见 crates/core/tests/cleanup_law_test.rs：retention 0
            // 时 maintenance 后各表行数不变）。旧实现 days=0 仍会执行
            // delete_closed_sessions_before(now)，把全部已关闭 sessions 删光
            // （实测 57 → 2）。days<=0（含负数，cutoff 会落到未来更危险）一律 no-op。
            if days <= 0 {
                println!("✓ cleanup {days}: 保留天数 0 表示永不清理, 未删除任何数据");
                return Ok(());
            }
            if with_events && days < 30 {
                return Err(Error::InvalidData(
                    "删除原始事件被拒绝: 天数必须 >= 30 且显式带 --yes".into(),
                ));
            }
            // 极限注入审查：同 export——极大 days 曾 panic（TimeDelta::days
            // out of bounds）。钳到 20 万天，cutoff 落到远古，语义不变（全删）。
            let days = days.min(200_000);
            let cutoff = (Utc::now() - Duration::try_days(days).unwrap_or(Duration::days(200_000)))
                .to_rfc3339();
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
    let mut days: u32 = 1;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--date" => {
                i += 1;
                date = args.get(i).cloned().unwrap_or_default();
            }
            "--days" => {
                i += 1;
                days = args
                    .get(i)
                    .and_then(|s| s.parse().ok())
                    .ok_or_else(|| Error::InvalidData("--days 需要一个正整数".into()))?;
                if days == 0 {
                    return Err(Error::InvalidData("--days 需要一个正整数（>=1）".into()));
                }
            }
            other => {
                return Err(Error::InvalidData(format!(
                    "未知选项: {other}（analyze 支持 --date/--days）"
                )))
            }
        }
        i += 1;
    }
    // --date 是窗口的最后一天（默认今天）；--days N 往前多看 N-1 天
    let end = parse_date(&date)?;
    let db_path = resolve_db();
    let conn = open_db(&db_path)?;
    let end_d = chrono::NaiveDate::parse_from_str(&end, "%Y-%m-%d")
        .map_err(|e| Error::InvalidData(format!("日期格式错: {e}")))?;
    let dates: Vec<String> = (0..days)
        .rev()
        .map(|off| {
            (end_d - chrono::Duration::days(i64::from(off)))
                .format("%Y-%m-%d")
                .to_string()
        })
        .collect();
    if dates.len() == 1 {
        print_analyze_day(&conn, &dates[0])?;
        return Ok(());
    }
    // 多日模式：逐日循环会随天数线性放大扫描成本（--days 7 实测 46.7s）。
    // 各天之间互不依赖，改为按天并行（每线程独立连接，busy_timeout=5000 兜底
    // 并发初始化；首个连接已在主线程完成迁移，后续各线程 open_db 不再竞争迁移写）。
    // 输出按日期顺序汇总打印；错误契约与旧逐日串行版一致：某天失败（含
    // open_db 失败）即以原错误变体（如 Error::Db）中止，失败日之后的输出
    // 不再打印——计算可并行，但汇总打印按日期序遇到首个错误即停。
    let workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .min(dates.len());
    let chunk = dates.len().div_ceil(workers);
    let db_path = &db_path; // 各线程共享路径引用，避免 move
                            // 各组并行计算，join 留在 scope 内；组序即日期序
    let joined = std::thread::scope(|scope| {
        let hs: Vec<_> = dates
            .chunks(chunk)
            .map(|group| {
                scope.spawn(move || -> Result<Vec<String>> {
                    // 每线程独立连接；连接/单日失败按原错误变体向上传播（不吞、
                    // 不字符串化），首个失败即中止本组后续天的计算
                    let conn = open_db(db_path)?;
                    let mut out = Vec::with_capacity(group.len());
                    for d in group {
                        out.push(analyze_day_text(&conn, d)?);
                    }
                    Ok(out)
                })
            })
            .collect();
        hs.into_iter()
            .map(|h| {
                h.join()
                    .map_err(|_| Error::InvalidData("分析线程 panic".into()))
            })
            .collect::<Vec<Result<Result<Vec<String>>>>>()
    });
    for r in joined {
        for text in r?.into_iter().flatten() {
            print!("{text}");
        }
    }
    Ok(())
}

/// analyze 的单日输出（多日模式逐日调用）。
fn print_analyze_day(conn: &Connection, date: &str) -> Result<()> {
    print!("{}", analyze_day_text(conn, date)?);
    Ok(())
}

/// analyze 单日文本（与旧 println! 逐行输出逐字节一致），供并行汇总后打印。
fn analyze_day_text(conn: &Connection, date: &str) -> Result<String> {
    let analysis = analyzer::analyze_day(conn, date)?;
    let anomalies = anomaly::detect_all(conn, date).unwrap_or_default();
    let mut out = String::new();
    use std::fmt::Write as _;
    let w = &mut out;
    writeln!(w, "=== Analyze {} ===", date)?;
    writeln!(w, "keys:           {}", analysis.total_keys)?;
    writeln!(w, "clicks:         {}", analysis.total_clicks)?;
    writeln!(w, "active minutes: {}", analysis.active_minutes)?;
    writeln!(w, "apm:            {:.1}", analysis.apm_avg)?;
    writeln!(w, "focus segments: {}", analysis.focus_segments.len())?;
    for s in &analysis.focus_segments {
        writeln!(
            w,
            "  {} → {}  {}min  keys={} clicks={} app={:?}",
            &s.start[11..16],
            &s.end[11..16],
            s.duration_min,
            s.key_count,
            s.click_count,
            s.app_name.as_deref().map(strip_c0)
        )?;
    }
    writeln!(
        w,
        "fragmentation:  {:.3} (longest {} min, {} breaks)",
        analysis.fragmentation.fragmentation_index,
        analysis.fragmentation.longest_streak_min,
        analysis.fragmentation.total_breaks
    )?;
    writeln!(w, "anomalies:      {}", anomalies.len())?;
    for a in &anomalies {
        writeln!(
            w,
            "  [{}] {} - {}",
            a.severity,
            a.kind,
            strip_c0(&a.message)
        )?;
    }
    Ok(out)
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
/// Wave20 P0：CLI autostart 开关同步写 settings.json——否则托盘下次启动
/// 按 settings 的 autostart 把注册表 Run 键写回去（CLI disable 被静默撤销）。
fn sync_settings_autostart(enable: bool) {
    let db = kynoptic_core::db::resolve_db_path();
    let mut st = crate::settings::load(&db);
    if st.autostart != enable {
        st.autostart = enable;
        if let Err(e) = crate::settings::save(&db, &st) {
            eprintln!("⚠ settings.json 同步失败: {e}（托盘启动时可能回写注册表）");
        }
    }
}

fn cmd_autostart(args: &[String]) -> Result<()> {
    let sub = args.first().map(|s| s.as_str()).unwrap_or("status");
    // 始终指向 digital-pulse.exe（同目录或 src-tauri\target\debug）
    let app_exe = locate_app_exe().unwrap_or_else(|| std::env::current_exe().unwrap_or_default());
    match sub {
        "enable" => {
            autostart::enable(&app_exe, &["--minimized"])?;
            sync_settings_autostart(true);
            println!("✓ 开机自启动已启用 (注册表 Run)");
            println!("  → {} --minimized", app_exe.display());
        }
        "disable" => {
            autostart::disable()?;
            sync_settings_autostart(false);
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
/// - RFC3339（统一转 UTC 规范形 `+00:00`、秒精度）/ `YYYY-MM-DDTHH:MM`（按本地时区补偏移）
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
            if let Ok(t) = chrono::DateTime::parse_from_rfc3339(s) {
                // timestamp 列全部由 DateTime<Utc>::to_rfc3339() 写入（+00:00 形），
                // 边界原样透传 +08:00 等显式偏移字面量，RFC3339 **字符串比较**
                // 的字典序不等于时间序，窗口会静默算错。镜像 mcp state.rs
                // normalize_bound 的修法：解析后转 UTC 规范形（+00:00、秒精度）。
                return Ok(t
                    .with_timezone(&Utc)
                    .to_rfc3339_opts(chrono::SecondsFormat::Secs, false));
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
                // Wave19：非法值报错而非静默回落 50（与 stats --days 同政策）
                let v: usize = args
                    .get(i)
                    .and_then(|s| s.parse().ok())
                    .ok_or_else(|| Error::InvalidData("--limit 需要一个正整数".into()))?;
                q.limit = v.clamp(1, 1000);
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
    // serve_stdio 只认 KYNOPTIC_DB 环境变量：把 --db 覆盖（或默认解析）经环境
    // 变量传入，修复 `kynoptic mcp --db X` 静默连错库的实测问题。
    std::env::set_var("KYNOPTIC_DB", resolve_db());
    kynoptic_mcp::serve_stdio();
    Ok(())
}

// === skill ===

/// SKILL.md 内容（随二进制内嵌，`skill install` 同步到各 AI 客户端 skill 目录）。
const SKILL_MD: &str = include_str!("assets/skill.md");

/// skill 目录约定：home 下的 `.zcode/.claude/.cursor` 三家，各 `skills/kynoptic/SKILL.md`。
const SKILL_CLIENT_DIRS: [&str; 3] = [".zcode", ".claude", ".cursor"];

/// Windows reparse point 属性位（symlink 与 junction 都带；std 的
/// `FileType::is_symlink` 对 junction 的返回语义历史上不稳定，直接查原始
/// 属性位最可靠——junction 是 IO_REPARSE_TAG_MOUNT_POINT，属性里必挂
/// FILE_ATTRIBUTE_REPARSE_POINT = 0x400）。
#[cfg(windows)]
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;

/// 路径存在且是 symlink/junction（reparse point）。
#[cfg(windows)]
fn is_reparse_point(p: &Path) -> bool {
    use std::os::windows::fs::MetadataExt;
    std::fs::symlink_metadata(p)
        .map(|md| md.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0)
        .unwrap_or(false)
}
#[cfg(not(windows))]
fn is_reparse_point(p: &Path) -> bool {
    std::fs::symlink_metadata(p)
        .map(|md| md.file_type().is_symlink())
        .unwrap_or(false)
}

/// 把内嵌 SKILL.md 写入 base 下的各客户端 skill 目录。返回写入路径列表。
/// 抽出 base 以便单测注入临时目录。
///
/// 安全防线（junction 跟随修复）：`skills\kynoptic` 若被换成指向任意目录的
/// junction/symlink（恶意软件可预建 `%USERPROFILE%\.claude` 等目录结构），
/// create_dir_all 会无声跟随、SKILL.md 直接写进目标处——这里先对目标目录
/// 做 reparse point 检查，命中即报错退出。写入用 tmp + rename 原子落盘：
/// 半程崩溃不会留下截断的 SKILL.md。
fn skill_install_to(base: &Path) -> Result<Vec<std::path::PathBuf>> {
    let mut written = Vec::new();
    for dir in SKILL_CLIENT_DIRS {
        let parent = base.join(dir).join("skills");
        std::fs::create_dir_all(&parent)
            .map_err(|e| Error::InvalidData(format!("创建 {} 失败: {e}", parent.display())))?;
        let target = parent.join("kynoptic");
        if is_reparse_point(&target) {
            return Err(Error::InvalidData(format!(
                "{} 是符号链接/junction，拒绝写入（防目录穿越；如非你本人设置请排查）",
                target.display()
            )));
        }
        std::fs::create_dir_all(&target)
            .map_err(|e| Error::InvalidData(format!("创建 {} 失败: {e}", target.display())))?;
        let file = target.join("SKILL.md");
        let tmp = target.join("SKILL.md.tmp");
        std::fs::write(&tmp, SKILL_MD)
            .map_err(|e| Error::InvalidData(format!("写入 {} 失败: {e}", tmp.display())))?;
        std::fs::rename(&tmp, &file)
            .map_err(|e| Error::InvalidData(format!("落盘 {} 失败: {e}", file.display())))?;
        written.push(file);
    }
    Ok(written)
}

/// `skill` 子命令入口：只接受 `skill install`，其他形式报 USAGE。
fn cmd_skill(args: &[String]) -> Result<()> {
    if args != ["install"] {
        return Err(Error::InvalidData(format!(
            "skill 子命令用法: kynoptic-ctl skill install（收到 {} 个参数）\n\n{USAGE}",
            args.len()
        )));
    }
    cmd_skill_install()
}

fn cmd_skill_install() -> Result<()> {
    let home = std::env::var("USERPROFILE")
        .map(std::path::PathBuf::from)
        .map_err(|_| Error::InvalidData("USERPROFILE 未设置，无法定位 skill 目录".into()))?;
    let files = skill_install_to(&home)?;
    println!("Kynoptic skill 已安装/更新到:");
    for f in &files {
        println!("  {}", f.display());
    }
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
                // Wave19：非法值报错而非静默回落 15（与 stats --days 同政策）
                secs = args
                    .get(i)
                    .and_then(|s| s.parse().ok())
                    .ok_or_else(|| Error::InvalidData("--secs 需要一个正整数".into()))?;
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

    // P1 单实例保护（实测：collect 与 tray 并发 = 同库双写、事件口径翻倍、
    // 双全局钩子）：与托盘同名 CreateMutexW（Wave29 挂账收口后名字由
    // kynoptic_core::singleton 单一事实源派生，含当前用户 SID），已有实例
    // 立即报错退出。
    acquire_single_instance(&kynoptic_core::singleton::singleton_mutex_name())?;

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
    // Wave20 P1：与 POST /api/settings 同一套校验（dash/src/lib.rs）——未知 id
    // 与空集都报错退出。手改 settings.json 写入空数组会绕过面板守卫，若不拦
    // 会静默 0 监控器空采（图标绿色但什么都没记）；要暂停请用托盘 Pause。
    let enabled: std::collections::HashSet<String> = if all {
        kynoptic_core::registry::all_monitor_ids()
            .iter()
            .map(|s| s.to_string())
            .collect()
    } else {
        let s = crate::settings::load(std::path::Path::new(&db_path));
        if let Some(bad) = crate::settings::first_invalid_id(&s.enabled_monitors) {
            return Err(Error::InvalidData(format!("未知监控器 id: {bad}")));
        }
        if s.enabled_monitors.is_empty() {
            return Err(Error::InvalidData(
                "enabled_monitors 不能为空（暂停请用托盘菜单）".into(),
            ));
        }
        s.enabled_monitors.into_iter().collect()
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

// === collect 单实例互斥体 ===

/// 单实例互斥体旧名（Wave29 挂账收口后仅作回退/测试锚点保留；生产路径
/// 一律走 kynoptic_core::singleton::singleton_mutex_name()——名字收敛为
/// core 单一事实源，杜绝三处字面值漂移）。
#[cfg(windows)]
#[allow(dead_code)]
const SINGLE_INSTANCE_MUTEX_NAME: &str = kynoptic_core::singleton::LEGACY_MUTEX_NAME;

/// 尝试持有单实例命名互斥体（可测：name 注入）。互斥体句柄故意持有到进程
/// 退出（RAII 释放会让保护在函数返回后失效）。已有实例 → Err。
#[cfg(windows)]
fn acquire_single_instance(name: &str) -> Result<()> {
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS};
    use windows_sys::Win32::System::Threading::CreateMutexW;
    fn hold(n: &str) -> Result<()> {
        let wname: Vec<u16> = n.encode_utf16().chain([0]).collect();
        unsafe {
            let h = CreateMutexW(std::ptr::null(), 0, wname.as_ptr());
            if h.is_null() {
                return Err(Error::InvalidData(format!(
                    "单实例互斥体创建失败 (GetLastError={}), 拒绝启动以防并发写库",
                    GetLastError()
                )));
            }
            // 只在句柄非空时才读 last error（创建成功不重置 last error,残留
            // ERROR_ALREADY_EXISTS 会误判）
            if GetLastError() == ERROR_ALREADY_EXISTS {
                CloseHandle(h);
                return Err(Error::InvalidData(
                    "已有 kynoptic 实例在运行(托盘或另一 collect): 两个采集器并发写同一数据库会导致事件翻倍, 拒绝启动".to_string(),
                ));
            }
        }
        Ok(())
    }
    hold(name)?;
    // 升级过渡桥：同时持有旧名互斥体，旧版 collect/tray（只探旧名）也能
    // 发现本实例；旧版实例尚在本会话运行时这里会报已存在，同样拒绝启动。
    // 只对生产新名（Global\ 前缀）生效——测试/注入的任意名字不做桥接。
    if name.starts_with("Global\\") {
        if let Some(legacy) = kynoptic_core::singleton::legacy_bridge_mutex_name(name) {
            hold(legacy)?;
        }
    }
    Ok(())
}

#[cfg(not(windows))]
fn acquire_single_instance(_name: &str) -> Result<()> {
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

// bridge_count 与三指标判定已下沉到 kynoptic-core（queries::bridge_count /
// queries::classify_minutes）——dash（overview/timeline）与 cli presence 共用
// 同一权威实现（剔注入、含点击、混合分钟双计、桥接读 settings）。

/// 前台分钟：窗口切换间隔累计（与 dash api_overview 同口径，不封顶）。
fn foreground_minutes(conn: &Connection, start: &str, end: &str) -> i64 {
    let mut fg_secs: i64 = 0;
    if let Ok(mut stmt) = conn.prepare(
        "SELECT timestamp FROM events \
          WHERE event_type = 'window' AND event_action = 'switch' \
            AND timestamp >= ?1 AND timestamp < ?2 ORDER BY timestamp",
    ) {
        if let Ok(rows) = stmt.query_map(params![start, end], |r| r.get::<_, String>(0)) {
            let stamps: Vec<String> = rows.flatten().collect();
            let parse = |t: &str| chrono::DateTime::parse_from_rfc3339(t).ok();
            for pair in stamps.windows(2) {
                if let (Some(a), Some(b)) = (parse(&pair[0]), parse(&pair[1])) {
                    // 采集停摆的超长间隔不归属（同 dash 侧，全库审查 P1）
                    let secs = (b - a).num_seconds();
                    if secs <= 2 * 3600 {
                        fg_secs += secs;
                    }
                }
            }
        }
    }
    fg_secs / 60
}

/// `kynoptic presence [--days N]`：每日 人在场/自动化/前台 三行式摘要。
/// 口径与 dashboard 三指标一致：三指标统一走 core 权威实现
/// queries::classify_minutes（与 dash overview/timeline 同一实现，无本地副本）。
fn cmd_presence(args: &[String]) -> Result<()> {
    let days = parse_presence_args(args)?;
    // 路径只解析一次：旧实现连调两次 resolve_db，默认推导分支会把
    // "using db: <path>" 打两遍，污染 stderr 且削弱人工核对信号。
    let db_path = resolve_db();
    let bridge = kynoptic_dash::settings::load(&db_path).presence_bridge_minutes;
    let conn = open_db(&db_path)?;
    println!("=== Presence last {days} day(s) (bridge <= {bridge} min) ===");
    for offset in (1 - days)..=0 {
        let date = queries::date_offset_str(offset);
        let Some((start, end)) = queries::local_day_range(&date) else {
            continue;
        };
        let day = queries::classify_minutes(&conn, &date, bridge);
        let foreground = foreground_minutes(&conn, &start, &end);
        println!("{date} presence:   {} min", day.presence_minutes);
        println!("{date} automation: {} min", day.automation_minutes);
        println!("{date} mixed:      {} min", day.mixed_minutes);
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
/// 看门狗互斥锁文件（exe 同目录，审查 P2：防两个 watchdog 进程并发
/// 读-改-写 watchdog-state.json——temp 写 + rename 的原子替换虽不会留下
/// 半截 JSON,但两个写者互相覆盖时失败计数/退避窗口仍会凭空回退,熔断
/// 可被并发写绕过）。create_new 独占创建是原子裁决点。
const WATCHDOG_LOCK_FILE: &str = "watchdog.lock";
/// 锁文件新鲜期：mtime 距今不超过该秒数视为另一个实例活跃;超过则认定
/// 上次进程崩溃未清理（所有正常退出路径都会删锁）,接管覆盖。
const WATCHDOG_LOCK_STALE_SECS: u64 = 120;
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
    std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|d| d.to_path_buf()))
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

/// 心跳内容解析（审查 P1：内容升级为 JSON `{"pid","ts","flush","stalled"}`，
/// 兼容旧版纯 RFC3339 文本）。返回 (内容时间戳, 是否 stalled)。
/// JSON 解析失败按旧格式回退——两代 tray 滚动升级期互不误判。
fn heartbeat_parse(content: &str) -> (Option<chrono::DateTime<Utc>>, bool) {
    let trimmed = content.trim();
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) {
        let ts = v
            .get("ts")
            .and_then(|t| t.as_str())
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|d| d.with_timezone(&Utc));
        let stalled = v.get("stalled").and_then(|s| s.as_bool()).unwrap_or(false);
        (ts, stalled)
    } else {
        (
            chrono::DateTime::parse_from_rfc3339(trimmed)
                .ok()
                .map(|d| d.with_timezone(&Utc)),
            false,
        )
    }
}

/// 心跳新鲜度判定：内容为 RFC3339 时间戳（或 JSON 的 ts 字段），返回距今秒数。
/// 缺失/解析失败返回 None（调用方按"过期"处理）；时钟回拨按 0 处理。
fn heartbeat_age_secs(now: chrono::DateTime<Utc>, content: &str) -> Option<i64> {
    heartbeat_parse(content)
        .0
        .map(|t| (now - t).num_seconds().max(0))
}

/// 心跳是否过期（缺失/不可解析/超龄/stalled 都算过期）。
/// 审查 P1：stalled=true 表示 tray 自报"采集器在跑但 writer 停滞超 1800s"，
/// 属采集挂死而非进程死亡——必须同样触发 kill/重启路径。
fn heartbeat_stale(now: chrono::DateTime<Utc>, content: Option<&str>) -> bool {
    match content {
        Some(c) => {
            let (_, stalled) = heartbeat_parse(c);
            if stalled {
                return true;
            }
            match heartbeat_age_secs(now, c) {
                Some(age) => age > HEARTBEAT_MAX_AGE_SECS,
                None => true,
            }
        }
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

/// 大小超阈值时执行轮转（改名覆盖 .old，日志是尽力而为语义）。
///
/// 审查 P2：并发句柄下 rename 会失败——Windows 上任何进程（含另一个
/// watchdog 实例）持有 append 打开的 watchdog.log 时改名即报错,旧实现
/// 单次失败即放弃,文件永远超限增长。改为重试 3 次（间隔 250ms,给并发
/// 句柄收尾窗口）;仍失败则退化为截断当前日志（OpenOptions truncate）——
/// 丢历史但保住日志通道可用。轮转各阶段失败均打 stderr 留痕（计划任务
/// 下无 console,尽力而为）。
fn rotate_log_if_needed(log_path: &Path) {
    let size = std::fs::metadata(log_path).map(|m| m.len()).unwrap_or(0);
    if !needs_log_rotation(size) {
        return;
    }
    let target = rotated_log_path(log_path);
    for attempt in 1..=3 {
        match std::fs::rename(log_path, &target) {
            Ok(()) => return,
            Err(e) => {
                eprintln!("watchdog: 日志轮转 rename 失败(第 {attempt}/3 次): {e}");
                if attempt < 3 {
                    std::thread::sleep(std::time::Duration::from_millis(250));
                }
            }
        }
    }
    // rename 三次均失败（并发句柄持续锁住）：截断当前日志兜底,新内容由
    // 调用方 watchdog_log 紧接着 append 写入,日志通道不中断。
    match std::fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(log_path)
    {
        Ok(_) => eprintln!("watchdog: rename 失败,已退化为截断 watchdog.log"),
        Err(e) => eprintln!("watchdog: 日志轮转完全失败(rename+truncate 均败): {e}"),
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
    /// 观察窗内跳过的重复拉起轮数（P1 状态机防重拉;仅诊断用,不参与判定）
    #[serde(default)]
    observation_skips: u64,
}

fn watchdog_state_path() -> PathBuf {
    exe_dir()
        .map(|d| d.join(WATCHDOG_STATE_FILE))
        .unwrap_or_else(|| PathBuf::from(WATCHDOG_STATE_FILE))
}

/// 看门狗锁文件路径（exe 同目录,与状态文件同锚点）
fn watchdog_lock_path() -> PathBuf {
    exe_dir()
        .map(|d| d.join(WATCHDOG_LOCK_FILE))
        .unwrap_or_else(|| PathBuf::from(WATCHDOG_LOCK_FILE))
}

/// 尝试获取看门狗锁（审查 P2）。返回 Some(锁路径) = 获得锁;None = 已有
/// 活跃实例,调用方应静默退出（不打日志不写状态,避免与在跑实例互踩）。
/// 规则:锁存在且 mtime < 120s → 另一实例活跃;锁存在且陈旧 → 上次崩溃
/// 残留,删除接管;create_new 独占创建失败（竞态输了）→ 视为活跃。
fn acquire_watchdog_lock() -> Option<PathBuf> {
    let path = watchdog_lock_path();
    if let Ok(meta) = std::fs::metadata(&path) {
        let fresh = meta
            .modified()
            .ok()
            .and_then(|t| t.elapsed().ok())
            .map(|d| d.as_secs() < WATCHDOG_LOCK_STALE_SECS)
            // mtime 不可得时保守视为活跃（宁可不跑,不能双跑）
            .unwrap_or(true);
        if fresh {
            return None;
        }
        let _ = std::fs::remove_file(&path);
    }
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .ok()
        .map(|_| path)
}

/// 释放看门狗锁（尽力而为;正常退出路径都必须调用,含 --once 单次模式）。
fn release_watchdog_lock(path: &Path) {
    let _ = std::fs::remove_file(path);
}

/// 读状态；文件缺失/损坏一律回退缺省（计数丢失可接受，不能因此拒绝工作）。
/// 反序列化后做值域钳制：state 文件只经 serde 类型校验，注入
/// consecutive_failures=u32::MAX 曾使 `+= 1` 回绕清零（熔断被永久绕过）、
/// 注入 epoch=i64::MIN 曾使裸减法 debug panic / release 回绕恒 Pending。
fn load_watchdog_state() -> WatchdogState {
    let mut st = std::fs::read_to_string(watchdog_state_path())
        .ok()
        .and_then(|s| serde_json::from_str::<WatchdogState>(&s).ok())
        .unwrap_or_default();
    // 失败计数钳到远小于 u32::MAX 的上限，saturating_add 永不回绕
    st.consecutive_failures = st.consecutive_failures.min(1_000_000);
    // epoch 类字段：非正数或超出当前时钟 1 天以上（时钟不可能合法走到那）
    // 一律重置为 0（= 无该值），后续判定按缺省语义走。backoff_until 允许
    // 最多 1 天未来（退避档位封顶 30min，1 天足够宽）。
    const MAX_EPOCH_SKEW_SECS: i64 = 86_400;
    let now = Utc::now().timestamp();
    for e in [
        &mut st.last_spawn_epoch,
        &mut st.heartbeat_at_spawn_epoch,
        &mut st.backoff_until_epoch,
    ] {
        if *e <= 0 || *e - now > MAX_EPOCH_SKEW_SECS {
            *e = 0;
        }
    }
    st
}

fn save_watchdog_state(st: &WatchdogState) {
    if let Ok(json) = serde_json::to_string(st) {
        let path = watchdog_state_path();
        // P1 原子写修复：先写 .tmp 再 rename 原子替换。旧实现直接 fs::write
        // 覆盖，进程被杀/断电时可能留下半截 JSON——load 侧虽有缺省兜底，
        // 但失败计数/退避窗口会凭空清零，熔断可被"写坏状态文件"绕过。
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, &json).is_ok() && std::fs::rename(&tmp, &path).is_ok() {
            return;
        }
        // 原子替换失败（tmp 写失败/rename 失败）兜底：尽力直接写（尽力而为语义不变）
        let _ = std::fs::write(&path, json);
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
    /// 疑似观察窗横跨了系统睡眠——本轮跳过，不计失败（审查 P2）
    SuspendSuspicion,
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
    // 审查 P2 判序修复：mtime 前进即成功，优先于观察窗判定。旧实现先看
    // 90s 墙钟再验 mtime——wall clock 穿越睡眠照常流逝，"睡眠期间拉起、
    // 醒来 mtime 明明已前进"也会先因 90s 未满被 Pending/误判。mtime 是墙
    // 钟盖章，拉起后哪怕只前进 1 秒也证明托盘心跳线程活着。
    if hb_mtime_epoch > hb_at_spawn_epoch {
        return SpawnOutcome::Recovered;
    }
    // saturating 减法：last_spawn_epoch 经值域钳制后仍防状态文件在两次
    // 读取间被换成极小值——裸减法 debug panic / release 回绕恒 Pending
    if now_epoch.saturating_sub(last_spawn_epoch) < SPAWN_GRACE_SECS {
        return SpawnOutcome::Pending;
    }
    // 审查 P2 睡眠误判守卫：mtime 未前进且墙钟已过观察窗时,若"拉起时刻的
    // 心跳 mtime → 现在"的墙钟跨度远超观察窗（> 4×90s）,大概率是机器在
    // 观察窗内睡了一觉（墙钟与文件时间一起跳变,托盘没机会写心跳）——
    // 跳过本次判定不记失败。上限 24×（约 36 分钟）兜底:真正秒死的托盘在
    // 无睡眠的长跨度下最终仍会走到 Failed,不会因本守卫永久豁免。
    if hb_at_spawn_epoch > 0
        && now_epoch.saturating_sub(hb_at_spawn_epoch) > SPAWN_GRACE_SECS * 4
        && now_epoch.saturating_sub(hb_at_spawn_epoch) <= SPAWN_GRACE_SECS * 24
    {
        return SpawnOutcome::SuspendSuspicion;
    }
    SpawnOutcome::Failed
}

/// 指数退避时长（纯函数）：失败次数未达阈值不退避（0）；达到后按档位
/// 2min → 8min → 30min 封顶。
fn backoff_delay_secs(consecutive_failures: u32) -> i64 {
    if consecutive_failures < FAILURE_THRESHOLD {
        return 0;
    }
    let idx =
        ((consecutive_failures - FAILURE_THRESHOLD) as usize).min(BACKOFF_STEPS_SECS.len() - 1);
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

/// 一轮看门狗 tick 的动作（纯状态机,单测覆盖;tray 不在时逐轮调用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TickAction {
    /// 无事可做（用户主动退出等）
    Idle,
    /// 允许拉起（调用方执行 spawn;成功后调 mark_spawned 记观察窗起点）
    Spawn,
    /// 熔断退避中,剩余秒数
    SkipBackoff(i64),
    /// 拉起观察窗（90s）内:禁止再次 spawn（P1 修复:旧实现观察窗内每 15s
    /// 无限重拉坏 tray 并覆盖观察窗起点,熔断永不可达）
    SkipObservation,
}

/// 观察窗内跳过的累计轮数（只用于日志,不入持久化状态）
const OBSERVATION_SKIP_LOG_EVERY: u32 = 4;

/// 看门狗一轮状态转移（纯函数）：先结算上一轮拉起结局,再决定本轮动作。
/// hb_mtime/hb_at_spawn 传 0 表示心跳从未刷新（stub 秒退场景）。
fn watchdog_tick(
    state: &mut WatchdogState,
    user_quit: bool,
    hb_mtime: i64,
    now_epoch: i64,
) -> TickAction {
    if user_quit {
        return TickAction::Idle;
    }
    // 先结算上一轮拉起的结局（观察窗 90s）
    match judge_spawn_outcome(
        state.last_spawn_epoch,
        now_epoch,
        hb_mtime,
        state.heartbeat_at_spawn_epoch,
    ) {
        SpawnOutcome::Pending => {
            // P1 关键：Pending 期间不得覆盖观察窗起点(last_spawn_epoch),也
            // 禁止再次 spawn——否则 90s 观察窗永远不成熟,consecutive_failures
            // 恒 0,坏 tray 每 15s 被无限重拉。跳过并计数。
            state.observation_skips = state.observation_skips.saturating_add(1);
            TickAction::SkipObservation
        }
        SpawnOutcome::Recovered => {
            // 心跳被刷新过 = 上轮拉起成功运行过；结束观察窗,允许新的拉起决策
            state.last_spawn_epoch = 0;
            match spawn_decision(
                state.consecutive_failures,
                state.backoff_until_epoch,
                now_epoch,
            ) {
                SpawnDecision::Spawn => TickAction::Spawn,
                SpawnDecision::SkipBackoff(r) => TickAction::SkipBackoff(r),
            }
        }
        SpawnOutcome::SuspendSuspicion => {
            // 审查 P2：疑似观察窗横跨系统睡眠——不记失败也不下结论,与
            // Pending 同样跳过本轮;托盘若活着,醒来后心跳 mtime 前进,下一
            // 轮自然走 Recovered。
            log::warn!("疑似睡眠唤醒，跳过本次判定");
            state.observation_skips = state.observation_skips.saturating_add(1);
            TickAction::SkipObservation
        }
        SpawnOutcome::Failed => {
            // saturating：release 下 u32::MAX+1 曾回绕为 0，熔断与退避被永久绕过
            state.consecutive_failures = state.consecutive_failures.saturating_add(1);
            state.last_spawn_epoch = 0;
            let delay = backoff_delay_secs(state.consecutive_failures);
            state.backoff_until_epoch = now_epoch + delay;
            match spawn_decision(
                state.consecutive_failures,
                state.backoff_until_epoch,
                now_epoch,
            ) {
                SpawnDecision::Spawn => TickAction::Spawn,
                SpawnDecision::SkipBackoff(r) => TickAction::SkipBackoff(r),
            }
        }
    }
}

/// 拉起成功后的观察窗起点记录（纯函数）。
fn mark_spawned(state: &mut WatchdogState, now_epoch: i64, hb_mtime: i64) {
    state.last_spawn_epoch = now_epoch;
    state.heartbeat_at_spawn_epoch = hb_mtime;
}

/// 睡眠唤醒守卫的复查判定（纯函数，单测覆盖）：首次发现异常后等待
/// STALE_RECHECK_WAIT_SECS 再复查。
///
/// 心跳读取四态语义（P1 修复"挂死 tray 永不重启"；审查 P1 增补 Stalled）：
/// - `HeartbeatRead::Age` — 文件可读、内容时间戳合法且未自报 stalled，携带距今年龄
/// - `HeartbeatRead::Stalled` — 内容合法（JSON）但 stalled=true：tray 进程活着、
///   心跳线程照常写,但采集 writer 停滞超 1800s（采集挂死）——按过期处理
/// - `HeartbeatRead::Missing` — 文件不存在
/// - `HeartbeatRead::Unparseable` — 文件存在但解析失败
///
/// 策略区分（tray 每 30s 全量重写心跳文件）：
/// - 复查回到阈值内 = 睡眠唤醒假象，放行；
/// - 复查仍超龄（无论是否继续增长）= 心跳 40s 未刷新且超龄，判挂死 kill；
/// - 复查仍 Stalled = 采集持续停滞（core last_flush_epoch 不前进），判挂死 kill；
/// - 复查仍 Missing = 写方已死或文件被删，未恢复，kill；
/// - 复查仍 Unparseable = tray 只写合法内容，垃圾说明写路径损坏，未恢复，kill。
///
/// 旧实现 `age_recheck > age_first && age_recheck > MAX` 在首查与复查都是
/// i64::MAX（文件持续不可读）时恒假，挂死 tray 永不 kill（4-8 小时空洞）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HeartbeatRead {
    /// 文件缺失
    Missing,
    /// 文件存在但内容非法
    Unparseable,
    /// 内容合法但 tray 自报采集停滞（stalled=true）
    Stalled,
    /// 合法时间戳，距今年龄（秒）
    Age(i64),
}

fn recheck_should_kill(_first: HeartbeatRead, recheck: HeartbeatRead) -> bool {
    match recheck {
        HeartbeatRead::Age(a) if a <= HEARTBEAT_MAX_AGE_SECS => false,
        // 仍超龄（含与首次持平的停滞）：两次间隔 40s 都异常,判挂死
        HeartbeatRead::Age(_) => true,
        // 审查 P1：复查仍 stalled = 采集持续停滞（进程活着也没用）,判挂死
        HeartbeatRead::Stalled => true,
        // 持续缺失/持续不可解析 = 未恢复（旧 bug 即漏掉这一分支）
        HeartbeatRead::Missing | HeartbeatRead::Unparseable => true,
    }
}

/// 心跳读取分类（纯函数）：content=None 即文件缺失;解析失败归 Unparseable
/// （首查按 Missing/Unparseable 都视同过期触发复查,区别只在日志与策略注释,
/// recheck 判定两者等价）。审查 P1：JSON 内容 stalled=true 归 Stalled——
/// 时间戳新鲜但采集挂死,同样走 kill/重启路径。
fn classify_heartbeat(content: Option<&str>, now: chrono::DateTime<Utc>) -> HeartbeatRead {
    match content {
        None => HeartbeatRead::Missing,
        Some(c) => {
            let (ts, stalled) = heartbeat_parse(c);
            match ts {
                Some(_) if stalled => HeartbeatRead::Stalled,
                Some(t) => HeartbeatRead::Age((now - t).num_seconds().max(0)),
                None => HeartbeatRead::Unparseable,
            }
        }
    }
}

/// 从心跳文本提取 `"pid":<u32>`（r25 混沌演练抽出为纯函数 + 单测）。
/// 仅接受十进制数字；带引号/负数/超 u32 一律 None——这些宽松解析失败的
/// 情况曾被用来回落按映像名全局击杀（实测误杀任意同名进程，见 kill_tray）。
fn heartbeat_pid(txt: &str) -> Option<u32> {
    txt.split("\"pid\":")
        .nth(1)
        .and_then(|rest| rest.split([',', '}']).next()?.trim().parse::<u32>().ok())
}

/// 该 pid 当前运行的映像名是否确为 kynoptic-tray.exe（查 tasklist，
/// 防止心跳文件被篡改后把任意 pid 当击杀目标——r25 实测：心跳里写什么
/// pid 就杀什么进程）。
#[cfg(target_os = "windows")]
fn pid_is_tray(pid: u32) -> bool {
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
        .stdin(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .to_lowercase()
                .contains("kynoptic-tray.exe")
        })
        .unwrap_or(false)
}

/// kill 托盘进程（心跳挂死时）。返回是否确实执行了一次成功的 taskkill。
///
/// r25 混沌演练 P0 修复：旧实现 (1) 心跳缺 pid 时回落 `taskkill /IM
/// kynoptic-tray.exe /T`——按映像名全局击杀，会误杀其他目录部署的托盘
/// （多实例并存/沙箱实验场景实测致生产托盘被杀）；(2) 解析到 pid 后
/// 不校验映像名直接杀——心跳文件被篡改即可击杀任意进程（实测杀掉无关
/// 进程）；(3) 用 `.status().is_ok()` 判成败——taskkill 退出码非零
/// （目标不存在/被拦截）也报成功，掩盖击杀失败。现改为：pid 必须存在、
/// 映像名必须是 kynoptic-tray.exe、以 taskkill 真实退出码为准，任一不
/// 满足都返回 false（watchdog 留痕"kill 失败"，下一轮重试）。
#[cfg(target_os = "windows")]
fn kill_tray() -> bool {
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    // 心跳与本 watchdog 同目录，属于"我们管理的那个托盘"
    let Ok(txt) = std::fs::read_to_string(
        std::env::current_exe()
            .ok()
            .and_then(|e| e.parent().map(|d| d.to_path_buf()))
            .unwrap_or_default()
            .join("kynoptic-heartbeat"),
    ) else {
        return false;
    };
    let Some(pid) = heartbeat_pid(&txt) else {
        return false;
    };
    if !pid_is_tray(pid) {
        return false;
    }
    Command::new("taskkill")
        .args(["/F", "/PID", &pid.to_string(), "/T"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
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
        // Wave29 挂账收口：探活名字与 tray/collect 同源（core 单一事实源，
        // 含当前用户 SID；跨会话同用户也互斥）
        let name: Vec<u16> = kynoptic_core::singleton::singleton_mutex_name()
            .encode_utf16()
            .chain([0])
            .collect();
        // 升级过渡桥：同时探旧名——watchdog 换新后仍要能为尚未升级的旧版
        // tray 探活（旧 tray 只持旧名，新 watchdog 只探新名会误判死亡并重复拉起）
        let legacy_name: Option<Vec<u16>> = kynoptic_core::singleton::legacy_bridge_mutex_name(
            &kynoptic_core::singleton::singleton_mutex_name(),
        )
        .map(|n| n.encode_utf16().chain([0]).collect());
        let exit_flag = exit_flag_path();
        let mut state = load_watchdog_state();
        // 审查 P2：并发 watchdog 互斥锁。拿不到锁 = 已有实例活跃,静默退出
        //（不写日志不写状态——写了也会和在跑实例互相覆盖）。锁在所有退出
        // 路径释放（含 --once 单次模式）。
        let lock_path = match acquire_watchdog_lock() {
            Some(p) => p,
            None => return Ok(()),
        };
        // 旗标告警去重：常驻模式下 tray-exit.flag 持续存在，只留痕一次
        let mut flag_warned = false;
        loop {
            let now_epoch = Utc::now().timestamp();
            let running = unsafe {
                let h = OpenMutexW(SYNCHRONIZE, 0, name.as_ptr());
                if !h.is_null() {
                    windows_sys::Win32::Foundation::CloseHandle(h);
                    true
                } else if let Some(lname) = &legacy_name {
                    let hl = OpenMutexW(SYNCHRONIZE, 0, lname.as_ptr());
                    if !hl.is_null() {
                        windows_sys::Win32::Foundation::CloseHandle(hl);
                        true
                    } else {
                        false
                    }
                } else {
                    false
                }
            };
            if running {
                // 进程活着：检查心跳。缺失或超龄（默认 180s）说明采集主循环
                // 可能挂死——但先做睡眠唤醒守卫（P1：系统睡眠期间心跳自然
                // 超龄，直接 kill 属误杀），40s 复查仍超龄才 kill。
                // 心跳读取三态（P1：区分缺失/解析失败,持续不可读 = 未恢复）
                let hb_read = std::fs::read_to_string(heartbeat_path())
                    .map(|s| s.trim().to_string())
                    .ok();
                let hb_first = classify_heartbeat(hb_read.as_deref(), Utc::now());
                let first_stale = heartbeat_stale(Utc::now(), hb_read.as_deref());
                if first_stale {
                    // 睡眠唤醒守卫：等待 40s 后复查（纯函数 recheck_should_kill）
                    // 审查 P1：stalled=true（tray 自报采集停滞）也走此复查路径
                    watchdog_log(&format!(
                        "心跳缺失/不可读/超龄(>{HEARTBEAT_MAX_AGE_SECS}s)/采集停滞(stalled), 疑似睡眠唤醒/挂死, {STALE_RECHECK_WAIT_SECS}s 后复查"
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
                        let hb_recheck_read = std::fs::read_to_string(heartbeat_path())
                            .map(|s| s.trim().to_string())
                            .ok();
                        let hb_recheck = classify_heartbeat(hb_recheck_read.as_deref(), Utc::now());
                        if recheck_should_kill(hb_first, hb_recheck) {
                            watchdog_log(&format!(
                                "复查仍未恢复(首次 {hb_first:?} → 复查 {hb_recheck:?}), 判定采集挂死, kill kynoptic-tray 以重启"
                            ));
                            if !kill_tray() {
                                // 审查 P2：杀失败（AV/权限）不留痕的话，后续每
                                // 个 --once 周期都重复 40s 睡眠+复查，且托盘
                                // 永远不会被真正重启。
                                watchdog_log(
                                    "kill kynoptic-tray 失败(taskkill 非零)，可能被安全软件拦截",
                                );
                            }
                        } else {
                            watchdog_log(&format!(
                                "复查时心跳已恢复(首次 {hb_first:?} → 复查 {hb_recheck:?}), 放行(睡眠唤醒假象)"
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
                // P1 状态机：结算上一轮结局 + 决定本轮动作。观察窗（Pending）
                // 内禁止重拉且不覆盖观察窗起点,坏 tray 秒退 3 次后必入退避。
                match watchdog_tick(&mut state, false, heartbeat_mtime_epoch(), now_epoch) {
                    TickAction::Idle => {}
                    TickAction::SkipObservation => {
                        if state
                            .observation_skips
                            .is_multiple_of(u64::from(OBSERVATION_SKIP_LOG_EVERY))
                        {
                            watchdog_log(&format!(
                                "拉起观察窗({SPAWN_GRACE_SECS}s)内, 跳过重复拉起(已跳过 {} 轮)",
                                state.observation_skips
                            ));
                        }
                    }
                    TickAction::SkipBackoff(remaining) => {
                        // Failed 结算发生在 tick 内（纯函数不落盘）,这里持久化
                        save_watchdog_state(&state);
                        watchdog_log(&format!(
                            "连续失败 {} 次, 熔断退避中, 约 {}s 后重试拉起",
                            state.consecutive_failures, remaining
                        ));
                    }
                    TickAction::Spawn => {
                        // Failed 结算后的首次拉起也把新计数落盘（防杀进程绕过熔断）
                        if state.last_spawn_epoch == 0 {
                            save_watchdog_state(&state);
                        }
                        if let Ok(exe) = std::env::current_exe() {
                            if let Some(dir) = exe.parent() {
                                let tray = dir.join("kynoptic-tray.exe");
                                // 便携版劫持缓解（低危，最小改动）：拒绝拉起
                                // 符号链接/junction 形式的托盘 exe。同用户可写
                                // 目录内 exe 被整体替换属部署形态固有权限问题，
                                // 完整防御需托盘侧签名/基线哈希校验（域外）。
                                if is_reparse_point(&tray) {
                                    watchdog_log(
                                        "kynoptic-tray.exe 是符号链接/junction，拒绝拉起（防劫持，请排查）",
                                    );
                                } else if tray.exists() {
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
                                            mark_spawned(
                                                &mut state,
                                                now_epoch,
                                                heartbeat_mtime_epoch(),
                                            );
                                            save_watchdog_state(&state);
                                        }
                                        Err(e) => {
                                            // 审查 P2：spawn 失败此前只打 stderr
                                            //（计划任务下无人看见），state 不动
                                            // → 判定 Recovered → 下一分钟再 Spawn，
                                            // 无限 1/min 循环且退出码恒 0。计入
                                            // 失败走既有退避熔断。
                                            let msg = format!("watchdog: 拉起托盘失败: {e}");
                                            eprintln!("{msg}");
                                            watchdog_log(&msg);
                                            state.consecutive_failures =
                                                state.consecutive_failures.saturating_add(1);
                                            if state.consecutive_failures >= FAILURE_THRESHOLD {
                                                let backoff =
                                                    backoff_delay_secs(state.consecutive_failures);
                                                state.backoff_until_epoch = now_epoch + backoff;
                                                watchdog_log(&format!(
                                                    "连续失败 {} 次，进入 {}s 退避",
                                                    state.consecutive_failures, backoff
                                                ));
                                            }
                                            save_watchdog_state(&state);
                                        }
                                    }
                                    if tray.exists() {
                                        watchdog_log("托盘不在且非用户退出,已拉起");
                                    } else {
                                        // 审查 P2：托盘 exe 缺失此前每分钟静默
                                        // 空转，不留任何痕迹。
                                        watchdog_log("kynoptic-tray.exe 缺失，无法拉起");
                                    }
                                }
                            }
                        }
                    }
                }
            } else if !flag_warned {
                // 静默停摆告警（低危，最小缓解）：tray-exit.flag 是固定名
                // 文件，任何同用户进程写一个同名文件即可让看门狗永久停止
                // 拉起（唯一不会被自动复活的停摆路径）。同用户可伪造内容，
                // 读取侧校验不可行——至少留痕一次供人工判断；旗标唯一合法
                // 删除点是托盘自身启动，被压制后无人清理。
                flag_warned = true;
                let age = std::fs::metadata(&exit_flag)
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.elapsed().ok())
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                watchdog_log(&format!(
                    "tray-exit.flag 已存在（{}s 前写入），看门狗不拉起托盘；若非本人主动退出托盘，请删除该文件",
                    age
                ));
            }
            if once {
                release_watchdog_lock(&lock_path);
                return Ok(());
            }
            // 锁续命（回归审查 P1）：锁只在启动时创建、从不刷新——120s 后任何
            // 并发 --once 都会把它当"崩溃残留"抢走，双实例互杀互拉。常驻循环
            // 每轮重写锁文件刷新 mtime。
            let _ = std::fs::write(&lock_path, Utc::now().to_rfc3339());
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

/// 子命令名定位：参数里第一个非 `--db` 且非"`--db` 的值"的元素下标。
/// 修复位置陷阱：`kynoptic --db X presence` 旧实现取 args[0] 当子命令，
/// 报"未知子命令 --db"。纯函数，单测覆盖。
fn find_subcommand(args: &[String]) -> Option<usize> {
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--db" {
            // 成对跳过 --db 及其值；尾部悬挂的 --db（缺值）也一并跳过
            i += 2;
            continue;
        }
        return Some(i);
    }
    None
}

fn main() -> ExitCode {
    // 日志全景审查 P1：CLI 此前从不初始化 logger（env_logger 挂在依赖里
    // 却没人 init），db 迁移/维护/欠聚合自愈/dash 服务的所有 log::warn!/
    // error! 全部落空。console 子命令补 stderr 输出（默认 warn 起，RUST_LOG
    // 可调）；`kynoptic mcp` 走 stdout 协议不受影响（env_logger 写 stderr）。
    // collect 子命令内部的 env_logger try_init 在此之后会静默让位。
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn"))
        .try_init()
        .ok();
    let args: Vec<String> = std::env::args().skip(1).collect();
    // --version/-V：update.rs 的 verify_launch 依赖 exit 0 判定自更新成功
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("kynoptic {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }
    let sub_idx = find_subcommand(&args);
    let sub: String = sub_idx
        .map(|i| args[i].clone())
        .unwrap_or_else(|| "help".to_string());
    // 全局 --db（审查 P2）：所有子命令可用，摘出后写入 DB_OVERRIDE 覆盖
    // resolve_db()。rest 先去掉子命令本身，使 --db 在任意位置都被成对摘除。
    // dashboard 自带 --db 解析，保持原样跳过。
    let mut rest: Vec<String> = args
        .iter()
        .enumerate()
        .filter(|(i, _)| Some(*i) != sub_idx)
        .map(|(_, v)| v.clone())
        .collect();
    let db_parsed = if sub == "dashboard" {
        Ok(())
    } else {
        extract_global_db(&mut rest)
    };
    let result: Result<()> = match db_parsed {
        Err(e) => Err(e),
        Ok(()) => match sub.as_str() {
            "collect" => cmd_collect(&rest),
            "stats" => cmd_stats(&rest),
            "export" => cmd_export(&rest),
            "report" => cmd_report(&rest),
            "db" => cmd_db(&rest),
            "analyze" => cmd_analyze(&rest),
            "ghost" => cmd_ghost(),
            "autostart" => cmd_autostart(&rest),
            "migrate" => cmd_migrate(&rest),
            "now" => cmd_now(&rest),
            "query" => cmd_query(&rest),
            "mcp" => cmd_mcp(),
            "skill" => cmd_skill(&rest),
            "probe" => cmd_probe(&rest),
            "dashboard" => dashboard::cmd_dashboard(&rest),
            "update" => {
                if rest.iter().any(|a| a == "--check") {
                    update::cmd_check_only()
                } else {
                    update::cmd_update(&rest)
                }
            }
            "watchdog" => cmd_watchdog(&rest),
            "presence" => cmd_presence(&rest),
            "help" | "-h" | "--help" => {
                print!("{}", USAGE);
                Ok(())
            }
            other => Err(Error::InvalidData(format!(
                "未知子命令: {other}\n\n{USAGE}"
            ))),
        },
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

    // === 全局 --db ===

    #[test]
    fn extract_global_db_consumes_pair_and_sets_override() {
        let mut a: Vec<String> = ["--days", "3", "--db", "x/y.db", "--json"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        extract_global_db(&mut a).unwrap();
        assert_eq!(
            a,
            vec!["--days".to_string(), "3".to_string(), "--json".to_string()]
        );
        assert_eq!(
            DB_OVERRIDE.lock().unwrap().as_ref(),
            Some(&PathBuf::from("x/y.db")),
            "--db 值必须写入全局覆盖"
        );
        // 缺路径：报错，不静默
        let mut b: Vec<String> = vec!["--db".to_string()];
        assert!(extract_global_db(&mut b).is_err());
        // 清理全局状态，避免影响其他用例
        *DB_OVERRIDE.lock().unwrap() = None;
    }

    // === 子命令定位（--db 任意位置） ===

    fn sv(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn find_subcommand_ignores_global_db_anywhere() {
        // 回归：`kynoptic --db X presence` 旧实现报"未知子命令 --db"
        assert_eq!(
            find_subcommand(&sv(&["--db", "x/y.db", "presence"])),
            Some(2)
        );
        assert_eq!(
            find_subcommand(&sv(&["presence", "--db", "x/y.db"])),
            Some(0)
        );
        // 子命令后的标志位（如 --days）会被当作"第一个非 --db 参数"：
        // 全局 --db 的正确用法是放在子命令前或后均可，但子命令本身必须是
        // 第一个非 --db/--db值 参数
        assert_eq!(
            find_subcommand(&sv(&["--days", "3", "--db", "x.db"])),
            Some(0)
        );
        assert_eq!(find_subcommand(&sv(&["collect"])), Some(0));
        // 悬挂 --db（缺值）：无可被子命令，交由 extract_global_db 报错
        assert_eq!(find_subcommand(&sv(&["--db"])), None);
        assert_eq!(find_subcommand(&sv(&[])), None);
    }

    // === report --date 别名 ===

    #[test]
    fn report_date_accepts_today_yesterday_aliases() {
        // 回归：`kynoptic report --date today` 旧实现报日期格式错
        let today = resolve_report_date("today").unwrap();
        assert_eq!(today, queries::today_local_str());
        let yesterday = resolve_report_date("yesterday").unwrap();
        assert_eq!(yesterday, queries::date_offset_str(-1));
        // 空串与纯日期行为不变
        assert_eq!(resolve_report_date("").unwrap(), queries::today_local_str());
        assert_eq!(resolve_report_date("2026-09-09").unwrap(), "2026-09-09");
        assert!(resolve_report_date("not-a-date").is_err());
    }

    // === skill install（junction 防护 + 原子写） ===

    #[test]
    fn skill_install_writes_files_without_tmp_leftover() {
        let base = std::env::temp_dir().join(format!("kynoptic-skill-ok-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let files = skill_install_to(&base).unwrap();
        assert_eq!(files.len(), 3);
        for f in &files {
            let content = std::fs::read_to_string(f).unwrap();
            assert!(!content.is_empty());
            // tmp + rename 原子写：不应留下 .tmp 残留
            let tmp = f.with_extension("md.tmp");
            assert!(!tmp.exists(), "不应残留 {tmp:?}");
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    #[cfg(windows)]
    #[test]
    fn skill_install_refuses_junction_target() {
        use std::process::Command;
        let base = std::env::temp_dir().join(format!("kynoptic-skill-j-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let secret = base.join("secret");
        std::fs::create_dir_all(&secret).unwrap();
        // 动态创建 junction（mklink /J，无需管理员权限）：把 .claude\skills\kynoptic
        // 换成指向 secret 的 junction——安装必须拒绝，而不是把文件写进 secret
        let junction = base.join(".claude").join("skills").join("kynoptic");
        std::fs::create_dir_all(junction.parent().unwrap()).unwrap();
        let out = Command::new("cmd")
            .args([
                "/C",
                "mklink",
                "/J",
                &junction.to_string_lossy(),
                &secret.to_string_lossy(),
            ])
            .output()
            .expect("mklink 运行失败");
        assert!(out.status.success(), "junction 创建失败（测试环境问题）");
        // 命中 reparse point：报错退出，secret 目录保持为空
        assert!(
            skill_install_to(&base).is_err(),
            "junction 目标必须拒绝写入"
        );
        assert!(std::fs::read_dir(&secret).unwrap().next().is_none());
        let _ = std::fs::remove_dir_all(&base);
    }

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
    fn parse_when_rfc3339_normalizes_to_utc_and_naive_datetime() {
        // RFC3339 边界统一转 UTC 规范形（+00:00、秒精度）再进 SQL：
        // 原样透传 +08:00 会按字典序比较、窗口静默算错（与 mcp normalize_bound 同修）
        let t = parse_when("2026-09-09T12:30:00+08:00", false).unwrap();
        assert_eq!(t, "2026-09-09T04:30:00+00:00");
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
        // 权威实现已下沉 core（dash/cli 共用），这里直接测 core 版本
        let bridge_count = queries::bridge_count;
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
        insert_event(
            &conn,
            "2026-09-13T10:00:30+08:00",
            "keyboard",
            "input_agg",
            r#"{"keys":10}"#,
        );
        // 10:07 全注入 -> 自动化
        insert_event(
            &conn,
            "2026-09-13T10:07:00+08:00",
            "keyboard",
            "input_agg",
            r#"{"keys":5,"injected_keys":5}"#,
        );
        // 混合分钟（human=8>0 且 injected=2>0）-> 同时计入在场与自动化
        insert_event(
            &conn,
            "2026-09-13T10:08:00+08:00",
            "keyboard",
            "input_agg",
            r#"{"keys":10,"injected_keys":2}"#,
        );
        // 窗口切换:10:00 -> 11:00 = 60 分钟前台
        insert_event(&conn, "2026-09-13T10:00:00+08:00", "window", "switch", "");
        insert_event(&conn, "2026-09-13T11:00:00+08:00", "window", "switch", "");
        let (start, end) = queries::local_day_range("2026-09-13").unwrap();
        let day = queries::classify_minutes(&conn, "2026-09-13", 2);
        let f = foreground_minutes(&conn, &start, &end);
        assert_eq!(
            day.automation_minutes, 2,
            "注入分钟 10:07 + 混合分钟 10:08 = 2"
        );
        assert_eq!(day.mixed_minutes, 1, "混合分钟双计，单独返回");
        // 大间隙按 dash 同款公式只补 bridge+1 步长（cap 后为 3）: 1 + 3 = 4
        assert_eq!(
            day.presence_minutes, 4,
            "两个在场分钟 + 间隙按 bridge 上限补步长（与 dash 口径一致）"
        );
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
        let day = queries::classify_minutes(&conn, "2026-09-13", 2);
        assert_eq!(day.automation_minutes, 0);
        assert_eq!(
            day.presence_minutes, 6,
            "10:00-10:01 + 桥接 10:02-10:03 + 10:04-10:05 = 6 分钟"
        );
    }

    #[test]
    fn presence_metrics_counts_scroll_wheel_as_human() {
        // Wave21 定案：滚轮滚动 = 人在主动阅读，计入人在场
        let conn = setup_presence_db();
        insert_event(
            &conn,
            "2026-09-13T10:00:30+08:00",
            "mouse",
            "input_agg",
            r#"{"clicks":0,"scroll_ticks":42}"#,
        );
        let (start, end) = queries::local_day_range("2026-09-13").unwrap();
        let _ = (start, end);
        let day = queries::classify_minutes(&conn, "2026-09-13", 2);
        assert_eq!(day.presence_minutes, 1, "纯滚轮分钟必须计入人在场");
        assert_eq!(day.automation_minutes, 0);
        // first_activity 不断言具体钟面值：CI 与本机时区不同，"HH:MM"
        // 是本地时区格式化结果（分钟数断言已覆盖语义）
        assert!(day.first_activity.is_some());
    }

    #[test]
    fn presence_metrics_empty_day_is_zero() {
        let conn = setup_presence_db();
        let (start, end) = queries::local_day_range("2026-09-13").unwrap();
        let day = queries::classify_minutes(&conn, "2026-09-13", 2);
        assert_eq!(
            (
                day.presence_minutes,
                day.automation_minutes,
                foreground_minutes(&conn, &start, &end)
            ),
            (0, 0, 0)
        );
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
        // 基准取真实 unix 秒量级:审查 P2 的睡眠守卫以 4×观察窗(360s)为下界,
        // 旧的 1000/2000 小整数会撞进守卫窗口被豁免,必须用真实尺度测 Failed。
        let base = 1_700_000_000i64;
        let hb_at_spawn = base;
        // 无进行中的拉起 → Recovered（无观察窗）
        assert!(matches!(
            judge_spawn_outcome(0, base + 1000, 0, 0),
            SpawnOutcome::Recovered
        ));
        // 观察窗未到 → Pending（即使心跳没刷新也不下结论）
        assert!(matches!(
            judge_spawn_outcome(base, base + SPAWN_GRACE_SECS - 1, base, hb_at_spawn),
            SpawnOutcome::Pending
        ));
        // 审查 P2 判序修复：mtime 在拉起后前进 → 立即 Recovered,哪怕墙钟
        // 观察窗未满（旧实现先看 90s 墙钟,睡眠横跨观察窗时会误判）
        assert!(matches!(
            judge_spawn_outcome(base, base + SPAWN_GRACE_SECS - 30, base + 5, hb_at_spawn),
            SpawnOutcome::Recovered
        ));
        // 观察窗已过 + 心跳被刷新过 → Recovered（托盘跑起来过）
        assert!(matches!(
            judge_spawn_outcome(base, base + SPAWN_GRACE_SECS, base + 1, hb_at_spawn),
            SpawnOutcome::Recovered
        ));
        // 观察窗已过 + 心跳从未刷新 + 跨度落在睡眠守卫窗口(4×,24×]内 →
        // SuspendSuspicion（疑似观察窗横跨睡眠,跳过不记失败）。
        // 注意跨度从 hb_at_spawn 起算：这里用 5×观察窗（在 (4×,24×] 内）。
        assert!(matches!(
            judge_spawn_outcome(base, base + SPAWN_GRACE_SECS * 5, base, hb_at_spawn),
            SpawnOutcome::SuspendSuspicion
        ));
        // 跨度恰在观察窗刚过但守卫下界之前（<4×）→ 正常结算 Failed
        assert!(matches!(
            judge_spawn_outcome(base, base + SPAWN_GRACE_SECS * 2, base, hb_at_spawn),
            SpawnOutcome::Failed
        ));
        // 跨度超出守卫上限（> 24×观察窗）:真死托盘最终必须结算 Failed
        assert!(matches!(
            judge_spawn_outcome(base, base + SPAWN_GRACE_SECS * 30, base, hb_at_spawn),
            SpawnOutcome::Failed
        ));
        assert!(matches!(
            judge_spawn_outcome(base, base + 5000, 0, hb_at_spawn),
            SpawnOutcome::Failed
        ));
    }

    #[test]
    fn judge_spawn_suspicion_window_bounds() {
        // 审查 P2 睡眠守卫边界：跨度落在 (4×, 24×] 观察窗内跳过判定;
        // hb_at_spawn==0（心跳从未存在的 stub 场景）不豁免,照常 Failed。
        let base = 1_700_000_000i64;
        assert!(matches!(
            judge_spawn_outcome(base, base + SPAWN_GRACE_SECS * 4 + 1, base, base),
            SpawnOutcome::SuspendSuspicion
        ));
        assert!(matches!(
            judge_spawn_outcome(base, base + SPAWN_GRACE_SECS * 24, base, base),
            SpawnOutcome::SuspendSuspicion
        ));
        assert!(matches!(
            judge_spawn_outcome(base, base + SPAWN_GRACE_SECS * 24 + 1, base, base),
            SpawnOutcome::Failed
        ));
        assert!(matches!(
            judge_spawn_outcome(base, base + SPAWN_GRACE_SECS * 30, 0, 0),
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
            observation_skips: 7,
        };
        let json = serde_json::to_string(&st).unwrap();
        let back: WatchdogState = serde_json::from_str(&json).unwrap();
        assert_eq!(back.consecutive_failures, 4);
        assert_eq!(back.backoff_until_epoch, 9999);
        assert_eq!(back.observation_skips, 7);
        // 损坏/缺字段：serde default 兜底 + 顶层回退缺省
        let partial: WatchdogState = serde_json::from_str(r#"{"consecutive_failures":2}"#).unwrap();
        assert_eq!(partial.consecutive_failures, 2);
        assert_eq!(partial.last_spawn_epoch, 0);
        assert_eq!(partial.observation_skips, 0, "新增字段缺省 0");
        assert!(serde_json::from_str::<WatchdogState>("garbage").is_err());
    }

    // === watchdog 状态机（P1：观察窗内禁止重拉,秒退 3 次必入退避） ===

    /// 模拟"坏 tray 秒退"：每轮 tick 间隔 15s,心跳 mtime 恒 0（从未刷新）。
    #[test]
    fn watchdog_tick_exiting_tray_spawns_three_times_then_backs_off() {
        let mut st = WatchdogState::default();
        let mut spawns: u32 = 0;
        let mut spawns_epoch: Vec<i64> = Vec::new();
        // 基准 1e6（unix 秒量级;0 是"无进行中拉起"哨兵,真实时间戳不为 0）
        let t0 = 1_000_000;
        for t in (0..390).step_by(15) {
            let now = t0 + t;
            match watchdog_tick(&mut st, false, 0, now) {
                TickAction::Spawn => {
                    spawns += 1;
                    spawns_epoch.push(now);
                    mark_spawned(&mut st, now, 0);
                }
                TickAction::SkipObservation => {
                    // 观察窗内:观察窗起点必须原样保留（P1 修复点）
                    assert_ne!(st.last_spawn_epoch, now, "Pending 轮不得覆盖观察窗起点");
                }
                TickAction::SkipBackoff(_) | TickAction::Idle => {}
            }
        }
        assert_eq!(
            spawns, 3,
            "秒退场景只允许 3 次拉起,之后必须熔断: {spawns_epoch:?}"
        );
        assert_eq!(st.consecutive_failures, 3);
        assert!(st.backoff_until_epoch > 0, "3 连败必须进入退避");
        assert!(
            spawns_epoch
                .windows(2)
                .all(|w| w[1] - w[0] >= SPAWN_GRACE_SECS),
            "两次拉起至少间隔一个完整观察窗"
        );
    }

    #[test]
    fn watchdog_tick_healthy_tray_recovers_state() {
        // 心跳在拉起后被刷新（mtime 前进）→ 观察窗结束,计数清零由健康分支做
        let mut st = WatchdogState {
            consecutive_failures: 1,
            last_spawn_epoch: 100,
            heartbeat_at_spawn_epoch: 90,
            backoff_until_epoch: 0,
            observation_skips: 5,
        };
        // t=100+90=190 > 观察窗, hb_mtime=120 > 90 → Recovered → 允许再拉起
        assert!(matches!(
            watchdog_tick(&mut st, false, 120, 190),
            TickAction::Spawn
        ));
        assert_eq!(st.last_spawn_epoch, 0, "Recovered 后观察窗关闭");
    }

    #[test]
    fn watchdog_tick_user_quit_is_idle() {
        let mut st = WatchdogState::default();
        assert!(matches!(
            watchdog_tick(&mut st, true, 0, 1000),
            TickAction::Idle
        ));
    }

    #[test]
    fn watchdog_tick_observation_skips_counter_increments() {
        let mut st = WatchdogState::default();
        mark_spawned(&mut st, 1_000_000, 0);
        for t in [1_000_015i64, 1_000_030, 1_000_045] {
            assert!(matches!(
                watchdog_tick(&mut st, false, 0, t),
                TickAction::SkipObservation
            ));
        }
        assert_eq!(st.observation_skips, 3);
        assert_eq!(st.last_spawn_epoch, 1_000_000, "观察窗起点未被覆盖");
        assert_eq!(st.consecutive_failures, 0, "观察窗未成熟不得记失败");
    }

    // === watchdog 睡眠唤醒守卫（P1：持续不可读 = 未恢复） ===

    #[test]
    fn recheck_guard_spares_refreshed_heartbeat() {
        // 复查时年龄回落（心跳被重新 touch）→ 放行
        assert!(!recheck_should_kill(
            HeartbeatRead::Age(200),
            HeartbeatRead::Age(35)
        ));
        // 复查时年龄回到阈值内 → 放行
        assert!(!recheck_should_kill(
            HeartbeatRead::Age(200),
            HeartbeatRead::Age(HEARTBEAT_MAX_AGE_SECS)
        ));
        // 复查时年龄仍在增长且超龄 → kill
        assert!(recheck_should_kill(
            HeartbeatRead::Age(200),
            HeartbeatRead::Age(240)
        ));
        // 复查时超龄但停滞（与首次持平）：两次间隔 40s 均异常 → kill
        assert!(recheck_should_kill(
            HeartbeatRead::Age(200),
            HeartbeatRead::Age(200)
        ));
    }

    #[test]
    fn recheck_guard_kills_when_heartbeat_persistently_unreadable() {
        // P1 回归：旧实现 age_recheck(i64::MAX) > age_first(i64::MAX) 恒假,
        // 文件持续不可读的挂死 tray 永不 kill（4-8 小时空洞）。
        let first_missing = HeartbeatRead::Missing;
        let first_garbage = HeartbeatRead::Unparseable;
        // 文件持续缺失 → kill
        assert!(recheck_should_kill(first_missing, HeartbeatRead::Missing));
        // 首查垃圾、复查缺失（及反向）→ kill
        assert!(recheck_should_kill(first_garbage, HeartbeatRead::Missing));
        assert!(recheck_should_kill(
            first_missing,
            HeartbeatRead::Unparseable
        ));
        // 文件持续解析失败 → kill（tray 只写合法 RFC3339,垃圾 = 写路径损坏）
        assert!(recheck_should_kill(
            first_garbage,
            HeartbeatRead::Unparseable
        ));
        // 复查恢复可读且在阈值内 → 放行
        assert!(!recheck_should_kill(first_missing, HeartbeatRead::Age(10)));
    }

    #[test]
    fn classify_heartbeat_maps_missing_unparseable_age() {
        let now = Utc::now();
        assert_eq!(classify_heartbeat(None, now), HeartbeatRead::Missing);
        assert_eq!(
            classify_heartbeat(Some("garbage"), now),
            HeartbeatRead::Unparseable
        );
        assert!(matches!(
            classify_heartbeat(Some(&(now - Duration::seconds(5)).to_rfc3339()), now),
            HeartbeatRead::Age(5) | HeartbeatRead::Age(4)
        ));
    }

    #[test]
    fn classify_heartbeat_json_content_and_stalled() {
        // 审查 P1：JSON 心跳（tray 新格式）——ts 提供年龄,stalled 决定分类
        let now = Utc::now();
        let fresh = (now - Duration::seconds(10)).to_rfc3339();
        let healthy = format!(r#"{{"pid":1,"ts":"{fresh}","flush":0,"stalled":false}}"#);
        assert!(matches!(
            classify_heartbeat(Some(&healthy), now),
            HeartbeatRead::Age(10) | HeartbeatRead::Age(9)
        ));
        // stalled=true：时间戳再新鲜也归 Stalled（采集挂死,按过期处理）
        let stalled = format!(r#"{{"pid":1,"ts":"{fresh}","flush":123,"stalled":true}}"#);
        assert_eq!(
            classify_heartbeat(Some(&stalled), now),
            HeartbeatRead::Stalled
        );
        // 旧版纯时间戳内容仍兼容
        assert!(matches!(
            classify_heartbeat(Some(&fresh), now),
            HeartbeatRead::Age(10) | HeartbeatRead::Age(9)
        ));
        // heartbeat_stale 对 stalled 必须判过期（首查触发 40s 复查路径）
        assert!(heartbeat_stale(now, Some(&stalled)));
        assert!(!heartbeat_stale(now, Some(&healthy)));
        // 复查仍 stalled → kill;首查 stalled、复查恢复 → 放行
        assert!(recheck_should_kill(
            HeartbeatRead::Stalled,
            HeartbeatRead::Stalled
        ));
        assert!(!recheck_should_kill(
            HeartbeatRead::Stalled,
            HeartbeatRead::Age(5)
        ));
    }

    // === collect 单实例互斥体（P1） ===

    // r25 混沌演练：心跳 pid 提取的对抗输入（防篡改心跳→误杀任意进程）
    #[cfg(windows)]
    #[test]
    fn heartbeat_pid_accepts_only_plain_numbers() {
        let now = Utc::now();
        let fresh = now.to_rfc3339();
        assert_eq!(
            heartbeat_pid(&format!(r#"{{"pid":123,"ts":"{fresh}"}}"#)),
            Some(123)
        );
        // 带空格的数字（JSON 常见排版）仍可解析
        assert_eq!(
            heartbeat_pid(&format!(r#"{{"pid": 123, "ts":"{fresh}"}}"#)),
            Some(123)
        );
        // 字符串 pid：拒绝（旧实现会因此回落映像名全局击杀）
        assert_eq!(
            heartbeat_pid(&format!(r#"{{"pid":"123","ts":"{fresh}"}}"#)),
            None
        );
        // 负数 / 超 u32 / 缺字段：拒绝
        assert_eq!(
            heartbeat_pid(&format!(r#"{{"pid":-1,"ts":"{fresh}"}}"#)),
            None
        );
        assert_eq!(
            heartbeat_pid(&format!(r#"{{"pid":99999999999,"ts":"{fresh}"}}"#)),
            None
        );
        assert_eq!(heartbeat_pid(&format!(r#"{{"ts":"{fresh}"}}"#)), None);
        // 旧版纯时间戳文本（无 pid）：None
        assert_eq!(heartbeat_pid(&fresh), None);
        // 尾随 } 截断正常
        assert_eq!(heartbeat_pid(r#"{"pid":42}"#), Some(42));
    }

    #[cfg(windows)]
    #[test]
    fn single_instance_mutex_name_matches_tray_contract() {
        // Wave29 挂账收口：名字不再锁字面值，改锁"三处同源"——collect 与
        // watchdog 都经 kynoptic_core::singleton::singleton_mutex_name()
        // 派生（Global\KynopticTrayMutex-<SID>，单层名字），旧名仅作回退。
        assert_eq!(
            kynoptic_core::singleton::singleton_mutex_name(),
            kynoptic_core::singleton::singleton_mutex_name()
        );
        assert_eq!(
            SINGLE_INSTANCE_MUTEX_NAME,
            kynoptic_core::singleton::LEGACY_MUTEX_NAME
        );
        let name = kynoptic_core::singleton::singleton_mutex_name();
        if kynoptic_core::singleton::current_user_sid().is_some() {
            assert!(
                name.starts_with(r"Global\KynopticTrayMutex-S-1-"),
                "got {name:?}"
            );
            // 单层名字契约：真实 CreateMutexW 对 Global\A\B 报 PATH_NOT_FOUND
            assert_eq!(name.matches('\\').count(), 1, "got {name:?}");
        } else {
            assert_eq!(name, SINGLE_INSTANCE_MUTEX_NAME, "SID 不可用须回退旧名");
        }
    }

    #[cfg(windows)]
    #[test]
    fn acquire_single_instance_second_acquire_fails() {
        // 同进程内二次 CreateMutexW 同名互斥体返回 ERROR_ALREADY_EXISTS:
        // 第一次应成功,第二次（模拟 tray 已在跑）必须报错拒绝。
        assert!(acquire_single_instance("Local\\KynopticCtlTestMutex").is_ok());
        assert!(acquire_single_instance("Local\\KynopticCtlTestMutex").is_err());
    }

    // === CSV BOM（P2：Excel 中文乱码） ===

    #[test]
    fn csv_export_writes_utf8_bom_jsonl_does_not() {
        let dir = std::env::temp_dir().join(format!("kyn-bom-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let csv_path = dir.join("t.csv");
        {
            use std::io::Write;
            let f = std::fs::File::create(&csv_path).unwrap();
            let mut buf = std::io::BufWriter::new(f);
            buf.write_all(&UTF8_BOM).unwrap();
            let mut w = csv::Writer::from_writer(buf);
            w.write_record(["a", "标题"]).unwrap();
            w.flush().unwrap();
        }
        let bytes = std::fs::read(&csv_path).unwrap();
        assert_eq!(
            &bytes[..3],
            &[0xEF, 0xBB, 0xBF],
            "CSV 文件头必须是 UTF-8 BOM"
        );
        // jsonl 路径不走 BOM：用同样的裸 File::create + writeln 复现其实现
        let jsonl_path = dir.join("t.jsonl");
        {
            use std::io::Write;
            let mut f = std::fs::File::create(&jsonl_path).unwrap();
            writeln!(f, r#"{{"a":1}}"#).unwrap();
        }
        let jl = std::fs::read(&jsonl_path).unwrap();
        assert_ne!(&jl[..1], &[0xEF], "jsonl 不得加 BOM");
        let _ = std::fs::remove_dir_all(&dir);
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
        assert_eq!(
            rotated_log_path(Path::new("watchdog.log")),
            PathBuf::from("watchdog.log.old")
        );
    }

    // === export 脱敏（--redact opt-in）与缺省目录 ===

    // 默认导出 window_title 原文（不脱敏）；sanitize_window_title 仅在显式
    // --redact 时被调用。本测试锁定 opt-in 脱敏函数本身的语义。
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
    fn sanitize_window_title_preserves_whitespace_structure() {
        // P3 回归：旧实现 split_whitespace+join 把换行压平,redact 版备份
        // 永久损失。换行必须原样保留。
        assert_eq!(sanitize_window_title("line1\nline2"), "line1\nline2");
        // 换行 + URL 查询串剥离共存
        assert_eq!(
            sanitize_window_title("页面 https://a.com/p?token=x\n第二行\t制表符"),
            "页面 https://a.com/p\n第二行\t制表符"
        );
        // 多空格与首尾空白原样保留
        assert_eq!(sanitize_window_title("  a   b  "), "  a   b  ");
        // URL 前的空格在剥查询串后仍保留
        assert_eq!(
            sanitize_window_title("doc: https://x.io/a?q=1"),
            "doc: https://x.io/a"
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

    #[test]
    fn skill_install_writes_all_client_dirs_with_content() {
        let tmp = std::env::temp_dir().join(format!("kyn-skill-{}", std::process::id()));
        let files = skill_install_to(&tmp).expect("skill install should succeed");
        assert_eq!(files.len(), SKILL_CLIENT_DIRS.len());
        for f in &files {
            let content = std::fs::read_to_string(f).expect("SKILL.md written");
            assert!(
                content
                    .replace("\r\n", "\n")
                    .starts_with("---\nname: kynoptic"),
                "frontmatter intact in {f:?}"
            );
            assert!(content.contains("意图路由"));
        }
        std::fs::remove_dir_all(&tmp).ok();
    }
}
