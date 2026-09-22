//! kynoptic-dash — 本机只读 dashboard HTTP 服务（cli 与 tray 共用唯一实现）。
//!
//! 从 `crates/cli/src/dashboard.rs` 迁出（原为 bin 私有模块，tray 曾整份复制，
//! 属技术债；现统一为 lib crate，`kynoptic-ctl dashboard` 与 `kynoptic-tray`
//! 均调用 [`serve`]）。绑定 127.0.0.1 的极简 HTTP 服务（std::net 手写 handler，
//! 零新依赖、零 CDN，静态页 `dashboard.html` 经 `include_str!` 内嵌，完全离线）。
//!
//! **只读铁律**：数据库经 `kynoptic_mcp::state::open_reader`（SQLITE_OPEN_READ_ONLY）
//! 打开——不建表、不跑迁移、永不写 events；agg 缓存只经采集器/维护路径写。
//! tray 的采集器在同进程持有写连接：READ_ONLY 连接对 WAL 库天然兼容（多读一写），
//! 仅在非 WAL 库上回退纯 READ_ONLY 连接 + busy_timeout（照搬原 cli 策略）。
//! 唯一写路径是 POST /api/settings（写 settings.json，不触碰 events）。
//!
//! 端点（除 POST /api/settings 外全部 GET、JSON、只读）：
//! - `/`                             内嵌双语单页（Overview/Activity/Anomalies/Settings）
//! - `/api/summary?date=`            当日 active_minutes / keys / clicks / top_app
//! - `/api/timeline?hours=`          近 N 小时按本地小时桶的应用分布（top5 + other）
//! - `/api/anomalies?days=`          与 MCP `get_anomalies` 同一异常检测
//!   （marathon 桥接阈值读 settings 的 presence_bridge_minutes）
//! - `/api/status`                   今日日期 + 最新事件时间戳 + db 路径
//! - `/api/overview`                 本次会话 uptime / 今日事件数 / 启用监控器数 /
//!   DB 大小 / CPU / 内存 / 前台应用（复用 MCP `get_current_status` 同一数据面）
//! - `/api/heatmap?weeks=`           按本地日聚合的活跃度 `[{date,value}]`，缺数天补零
//! - `/api/apps?days=`               Top 应用排行（window 事件，空名排除）
//! - `/api/hours?date=`              指定日 24 小时逐时活动量（缺时补零）
//! - `/api/settings` (GET/POST)      设置读写（写 settings.json，不触碰 events）
//!
//! 无鉴权：仅绑定回环地址，不暴露到网络（页脚已声明）。

pub mod settings;

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::sync::Arc;

use chrono::Timelike;
use chrono::{DateTime, Local, TimeZone, Utc};
use rusqlite::{params, Connection};
use serde_json::{json, Value};

use kynoptic_core::queries;
use kynoptic_core::registry;
use kynoptic_core::{Error, Result};

use settings::AppSettings;

/// 设置纪元：每次 POST /api/settings 成功即 +1。tray 监听该值变化，
/// 自动用新设置重启采集器（保存即生效，无需手动重启）。
pub static SETTINGS_EPOCH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// 当前设置纪元（tray 轮询用）。
pub fn settings_epoch() -> u64 {
    SETTINGS_EPOCH.load(std::sync::atomic::Ordering::Relaxed)
}

/// 内嵌静态页（与产品 monospace/终端风一致的暗色双语单页）。
pub const DASHBOARD_HTML: &str = include_str!("dashboard.html");

pub const DEFAULT_PORT: u16 = settings::DEFAULT_DASHBOARD_PORT;

// ─── 数据面（&Connection / &Path 纯函数，可脱离 TCP 单测） ───────────────────

/// 400 错误回显净化（审查 P2）：原始输入直接拼进错误消息会被反射回页面/日志
/// （日志注入 + 存储型 XSS 的投放面）。只保留字母数字与 '-'，截到 32 字符，
/// 超长以 "…" 结尾提示被净化。
fn sanitize_date_echo(raw: &str) -> String {
    let kept: String = raw
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .take(32)
        .collect();
    if raw.len() > 32 || kept.len() != raw.chars().count() {
        format!("{kept}…")
    } else {
        kept
    }
}

/// 统一的日期参数错误消息（回显经 sanitize_date_echo 净化）。
fn date_err(raw: &str, hint: &str) -> String {
    format!("日期格式错: {}（应为 {hint}）", sanitize_date_echo(raw))
}

/// GET /api/summary?date= — 当日四卡数据。数字全部来自 DB。
/// 未来日期一律拒绝：查询"明天"拿到看似权威的全 0 比报错更误导
///（Wave18 P1：/api/summary、report、hours、apps_grid 统一口径）。
fn reject_future_date(date: &str) -> std::result::Result<(), String> {
    // 只对"形如日期"的输入做未来判定：字面量 today 与垃圾输入分别由
    // 各 api_* 的解析/净化路径处理（否则 "today" 会被误判为未来日期，
    // 垃圾输入会绕过净化直接回显）
    let is_date_like = date.len() == 10
        && date.as_bytes()[..4].iter().all(|b| b.is_ascii_digit())
        && date.as_bytes()[4] == b'-'
        && date.as_bytes()[5..7].iter().all(|b| b.is_ascii_digit())
        && date.as_bytes()[7] == b'-'
        && date.as_bytes()[8..].iter().all(|b| b.is_ascii_digit());
    if !is_date_like {
        return Ok(());
    }
    let today = queries::today_local_str();
    if date > today.as_str() {
        return Err(format!("date {date} 在未来（今天 {today}）"));
    }
    Ok(())
}

pub fn api_summary(conn: &Connection, date: &str) -> std::result::Result<Value, String> {
    let (start, end) =
        queries::local_day_range(date).ok_or_else(|| date_err(date, "YYYY-MM-DD"))?;
    let totals = queries::day_totals(conn, date);
    let top_app = queries::top_apps_today(conn, &start, &end, 1)
        .into_iter()
        .next()
        .map(|(app, _)| app);
    Ok(json!({
        "date": date,
        "active_minutes": totals.active_minutes,
        "keys": totals.keys,
        "clicks": totals.clicks,
        "top_app": top_app,
    }))
}

// 分钟分类 / 桥接 / 单日三指标已下沉到 kynoptic-core（queries::minute_classification
// / queries::bridge_count / queries::classify_minutes）——overview、timeline 与
// cli presence 共用同一权威实现（人在场口径：剔注入、含点击、混合分钟双计、
// 桥接读 settings），本 crate 不再持有副本。

/// 单小时桶内的应用事件分布，top 5 + 其余归并 other。
fn top5_with_other(mut apps: Vec<(String, i64)>) -> Vec<(String, i64)> {
    apps.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    if apps.len() <= 5 {
        return apps;
    }
    let other: i64 = apps[5..].iter().map(|(_, n)| n).sum();
    apps.truncate(5);
    apps.push(("(other)".into(), other));
    apps
}

/// GET /api/timeline?hours= — 近 `hours` 本地小时的事件分布，按小时桶。
/// `now` 注入以便硬件无关测试。只含有数据的桶，桶内应用 top5 + other。
///
/// human_min 口径（与 overview presence 一致，权威实现 queries::minute_classification
/// + queries::bridge_count）：
/// - `human_min` = 桥接后分钟数（向后兼容页面显示的字段名）
/// - `human_min_unbridged` = 未桥接的原始人在场分钟数
/// - `human_min_bridged` = 桥接后分钟数（与 human_min 同值，显式字段）
pub fn api_timeline_at(
    conn: &Connection,
    hours: u32,
    now: DateTime<Utc>,
    bridge_min: u32,
) -> std::result::Result<Value, String> {
    // Wave23：内部与 HTTP 层 clamp 对齐（原 48 与 8760 两层打架，
    // hours=100 被静默截成 48）。Wave24 放大实测：8760 桶在年量级库上
    // 3-4s + 2.2MB 响应，而小时粒度年视图本无使用场景——收口 744（31 天），
    // 更长跨度请用 /api/heatmap（daily_agg 预聚合，4ms）。
    let hours = hours.clamp(1, 744);
    // 边界按 UTC 计算后直接用于 WHERE（timestamp 列为 UTC RFC3339）
    let end = now;
    // Wave28 P0：窗口终点取当前本地整点（floor 到小时），起点 = 终点 −
    // (hours−1) 小时。旧实现按 now−hours 起步、整点步进循环 cur<end，
    // 恰好把"当前小时"整桶丢掉（cur+1h==end），仪表盘最新一行可滞后
    // 近 2 小时。现在恒返回恰好 hours 个桶，最后一桶是进行中的当前小时。
    let end_local_raw = end.with_timezone(&Local);
    let end_hour = end_local_raw
        .date_naive()
        .and_hms_opt(end_local_raw.hour(), 0, 0)
        .and_then(|n| Local.from_local_datetime(&n).single())
        .unwrap_or(end_local_raw);
    let start_hour = end_hour - chrono::Duration::hours(i64::from(hours - 1));
    let start = start_hour.with_timezone(&Utc);
    let off = local_offset_modifier_at(now);

    let mut stmt = conn
        .prepare(
            "SELECT substr(datetime(timestamp, ?1), 1, 13) AS hour_bucket, \
                    COALESCE(NULLIF(app_name, ''), NULLIF(window_title, ''), '(unknown)') AS app, \
                    COUNT(*) AS cnt \
             FROM events \
             WHERE timestamp >= ?2 AND timestamp < ?3 \
               AND event_type IN ('keyboard','mouse','window') \
               AND event_action != 'input_agg' \
             GROUP BY hour_bucket, app",
        )
        .map_err(|e| e.to_string())?;
    let rows: Vec<(String, String, i64)> = stmt
        .query_map(params![&off, start.to_rfc3339(), end.to_rfc3339()], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
            ))
        })
        .map_err(|e| e.to_string())?
        .flatten()
        .collect();

    // 三色分层（三指标模型）：每桶统计 人在场/自动化 的输入分钟数。
    // 与 overview 共用 queries::minute_classification 同一 SQL/口径（core 权威实现）。
    // 每桶另存人在场分钟（当日分钟坐标）供桥接计算。
    let mut minute_kind: std::collections::HashMap<String, (u32, u32)> =
        std::collections::HashMap::new();
    let mut human_min_of: std::collections::HashMap<String, Vec<i64>> =
        std::collections::HashMap::new();
    // 整窗人侧分钟（epoch 分钟）——供跨小时桥接（Wave20 P1）
    let mut all_human_min: Vec<i64> = Vec::new();
    let mrows = queries::minute_classification(conn, &start.to_rfc3339(), &end.to_rfc3339(), &off)?;
    for (minute_bucket, human, auto) in mrows {
        let hour_bucket = minute_bucket.replacen(' ', "T", 1)[..13].to_string();
        if human {
            // 本地分钟桶 "YYYY-MM-DD HH:MM" -> 当日第几分钟（同一本地日内比较）
            if let Ok(t) = chrono::NaiveDateTime::parse_from_str(&minute_bucket, "%Y-%m-%d %H:%M") {
                human_min_of
                    .entry(hour_bucket.clone())
                    .or_default()
                    .push(i64::from(t.hour()) * 60 + i64::from(t.minute()));
                // Wave28 P0：minute_bucket 是本地墙上钟（SQL datetime(ts, off) 的输出）。
                // 旧实现 and_utc() 把本地钟当 UTC，而桥接分钟归位
                // （local_hour_of_epoch_min 用 Local）又加回偏移，整批桥接分钟
                // 偏移 ±8h——实测 9 个小时桶 human_min 假 0、1 个桶吞掉别桶
                // 的桥接分钟。必须按本地时区换算 epoch。
                if let Some(lt) = Local.from_local_datetime(&t).single() {
                    all_human_min.push(lt.timestamp() / 60);
                }
            }
            let e = minute_kind.entry(hour_bucket.clone()).or_insert((0, 0));
            e.0 += 1;
        }
        if auto {
            let e = minute_kind.entry(hour_bucket).or_insert((0, 0));
            e.1 += 1;
        }
    }
    // (本地小时桶 "YYYY-MM-DDTHH", [(app, cnt)])
    let mut by_bucket: std::collections::BTreeMap<String, Vec<(String, i64)>> =
        std::collections::BTreeMap::new();
    for (bucket, app, cnt) in rows {
        // SQLite datetime() 输出 "YYYY-MM-DD HH"；统一成 ISO "YYYY-MM-DDTHH"
        let bucket = bucket.replacen(' ', "T", 1);
        by_bucket.entry(bucket).or_default().push((app, cnt));
    }
    // 整窗桥接展开：把 human 分钟之间的间隙（<= bridge）填上，得到"在场
    // 分钟全集"，再按小时归位。分钟坐标用本地 NaiveDateTime 以对齐小时桶。
    let bridge_gap = i64::from(bridge_min.min(15));
    let mut expanded: std::collections::BTreeMap<String, i64> = std::collections::BTreeMap::new();
    {
        all_human_min.sort_unstable();
        all_human_min.dedup();
        let human_set: std::collections::HashSet<i64> = all_human_min.iter().copied().collect();
        for &m in &all_human_min {
            *expanded
                .entry(local_hour_of_epoch_min(m, &off))
                .or_default() += 1;
            // 向后填洞：m+1..=m+gap 属于桥接分钟
            let mut n = m + 1;
            while n <= m + bridge_gap
                && !human_set.contains(&n)
                && all_human_min.last().is_some_and(|&last| n <= last)
            {
                *expanded
                    .entry(local_hour_of_epoch_min(n, &off))
                    .or_default() += 1;
                n += 1;
            }
        }
    }
    let bridged_per_hour = expanded;
    // 补零桶：固定返回窗口内全部本地小时，无数据小时 human/auto 全 0，
    // 前端可区分"没开机"与"开机没碰"。
    let mut buckets: Vec<Value> = Vec::new();
    let mut cur = start_hour;
    let end_local = end_hour;
    while cur <= end_local {
        let hour = cur.format("%Y-%m-%dT%H").to_string();
        let apps = by_bucket.remove(&hour).unwrap_or_default();
        let list: Vec<Value> = top5_with_other(apps)
            .into_iter()
            // 与 insights 卡一致：展示名剔除 ".exe" 后缀（如 "ZCode.exe" -> "ZCode"）
            .map(|(app, n)| json!({"app": app.trim_end_matches(".exe"), "events": n}))
            .collect();
        let (human, auto) = minute_kind.get(&hour).copied().unwrap_or((0, 0));
        let unbridged = human as i64;
        // 桥接（Wave20 P1）：先在整段窗口上桥接（跨小时连续段不再被小时
        // 边界截断，与 overview presence 总和一致），再把桥接后的分钟按
        // 所属小时归位。
        let bridged = bridged_per_hour
            .iter()
            .filter(|(h, _)| h.as_str() == hour)
            .map(|(_, n)| *n)
            .sum::<i64>();
        buckets.push(json!({
            "hour": hour,
            "apps": list,
            // human_min = 桥接后分钟数（字段名向后兼容页面显示）
            "human_min": bridged,
            "human_min_unbridged": unbridged,
            "human_min_bridged": bridged,
            "auto_min": auto,
        }));
        cur += chrono::Duration::hours(1);
    }
    // DST 口径说明：本地换算用的是响应生成时刻的偏移（见 local_offset_modifier_at），
    // 跨夏令时的历史小时桶可能有 ±1 小时偏移；把偏移随响应返回供前端标注。
    let offset_secs = now.with_timezone(&Local).offset().local_minus_utc();
    Ok(json!({
        "hours": hours,
        "generated_at": now.to_rfc3339(),
        "local_offset_seconds": offset_secs,
        "local_offset_note": "本地小时桶按响应生成时刻的 UTC 偏移换算；跨 DST 的历史小时桶可能有 ±1 小时偏移 / Local hour buckets use the UTC offset at generation time; DST transitions may shift historical buckets by ±1h.",
        "buckets": buckets,
    }))
}

/// 指定时刻的本地偏移修饰符（与 `queries::local_offset_modifier` 同口径，
/// 但允许测试注入时刻；当前时刻下两者一致）。
/// DST 隐患（审查 P2）：这里用单一偏移换算整段历史查询；跨夏令时的桶
/// 可能偏 ±1 小时。timeline 响应已带 local_offset_seconds/note 说明口径。
/// epoch 分钟 -> 本地小时桶 "YYYY-MM-DDTHH"（与 minute_classification 同一偏移）。
fn local_hour_of_epoch_min(epoch_min: i64, off: &str) -> String {
    // datetime(ts, off) 与 SQLite 同口径：先拼 RFC3339 再本地化
    let ts = chrono::DateTime::from_timestamp(epoch_min * 60, 0)
        .unwrap_or_default()
        .with_timezone(&Local)
        .format("%Y-%m-%dT%H")
        .to_string();
    let _ = off; // chrono Local 已含当前偏移（与调用侧 local_offset_modifier_at 同源）
    ts
}

fn local_offset_modifier_at(now: DateTime<Utc>) -> String {
    let secs = now.with_timezone(&Local).offset().local_minus_utc() as i64;
    format!(
        "{}{} seconds",
        if secs >= 0 { "+" } else { "-" },
        secs.abs()
    )
}

/// 从中文 message 中提取数字参数（按模板出现顺序），供英文参数化文案复用。
/// 只识别 ASCII 数字与小数点，逐字节扫描在 UTF-8 下安全（多字节序列的高位
/// 字节不会命中 is_ascii_digit），且数字段起点/终点均为 ASCII 边界。
fn message_numbers(message: &str) -> Vec<f64> {
    let mut out = Vec::new();
    let b = message.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i].is_ascii_digit() {
            let start = i;
            while i < b.len()
                && (b[i].is_ascii_digit()
                    || (b[i] == b'.' && i + 1 < b.len() && b[i + 1].is_ascii_digit()))
            {
                i += 1;
            }
            if let Ok(v) = message[start..i].parse::<f64>() {
                out.push(v);
            }
        } else {
            i += 1;
        }
    }
    out
}

/// 整数化的数字展示（模板里分钟数/事件数都是整数；倍率保留 1 位小数）。
fn fmt_num(v: f64) -> String {
    if v.fract() == 0.0 && v.abs() < 1e15 {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}

/// 异常 kind -> 中文友好标签（仪表盘直接展示，避免渲染原始 kind 码）。
fn kind_label(kind: &str) -> &'static str {
    match kind {
        "late_night" => "深夜活动",
        "apm_burst" => "APM 突增",
        "marathon" => "长时间连续在场",
        "new_app_surge" => "新应用激增",
        _ => "异常",
    }
}

/// 异常 kind + 中文 message -> 参数化英文文案（前端 message_en）。
/// 与中文模板同源：分钟数 / 倍率 / 应用名等数字全部带上，信息量对齐。
/// kynoptic-mcp 的同名函数只给静态模板；dash 侧拿到完整 message 后重新
/// 参数化，避免英文侧丢失 "240 分钟""13.2x" 这类关键数字。
fn anomaly_message_en(kind: &str, message: &str, at: Option<&str>) -> String {
    match kind {
        // "深夜活动：{n} 按键"
        "late_night" => format!(
            "Late-night activity: {} keystrokes after 23:00",
            fmt_num(message_numbers(message).first().copied().unwrap_or(0.0))
        ),
        // "APM 突增：{minute} 达到 {n}（历史均值 {avg} 的 {ratio:.1}x）"
        // 分钟时间戳本身含数字，从 "达到" 之后再取数（n, avg, ratio）。
        "apm_burst" => {
            let tail = message.split("达到").nth(1).unwrap_or(message);
            let n = message_numbers(tail);
            format!(
                "APM burst: {} hit {} ({:.1}x the {} historical average)",
                at.unwrap_or_default(),
                fmt_num(n.first().copied().unwrap_or(0.0)),
                n.get(2).copied().unwrap_or(0.0),
                fmt_num(n.get(1).copied().unwrap_or(0.0)),
            )
        }
        // "连续在场 {longest} 分钟（长时间无离开）"
        "marathon" => format!(
            "Continuous presence: {} minutes without leaving",
            fmt_num(message_numbers(message).first().copied().unwrap_or(0.0))
        ),
        // "应用使用突增：{app}（今天 {n}，日均 {avg}，{ratio:.1}x）"
        // "新应用首次出现：{app}（{n} 事件）"
        "new_app_surge" => {
            let app = message
                .split_once('：')
                .map(|(_, rest)| rest)
                .unwrap_or("")
                .split('（')
                .next()
                .unwrap_or("")
                .trim();
            let n = message_numbers(message);
            if message.contains("首次出现") {
                format!(
                    "New app first seen: {app} ({} events)",
                    fmt_num(n.first().copied().unwrap_or(0.0))
                )
            } else {
                format!(
                    "App usage surge: {app} (today {}, daily average {}, {:.1}x)",
                    fmt_num(n.first().copied().unwrap_or(0.0)),
                    fmt_num(n.get(1).copied().unwrap_or(0.0)),
                    n.get(2).copied().unwrap_or(0.0),
                )
            }
        }
        other => format!("Anomaly detected: {other}"),
    }
}

/// GET /api/anomalies?days= — 与 MCP get_anomalies 同一检测入口，但 marathon
/// 的连续性桥接阈值读 settings 的 `presence_bridge_minutes`（全站连续性口径
/// 统一源），每条按中文 message 重新参数化 message_en。
pub fn api_anomalies(conn: &Connection, days: u32, db_path: &Path) -> Value {
    let bridge = settings::load(db_path).presence_bridge_minutes.min(15);
    let days = days.clamp(1, 30) as usize;
    let mut out: Vec<Value> = Vec::new();
    for i in 0..days {
        let date = queries::date_offset_str(-(i as i64));
        let list =
            kynoptic_core::anomaly::detect_all_with_bridge(conn, &date, bridge).unwrap_or_default();
        for a in list {
            if out.len() >= 100 {
                return json!({"anomalies": out, "truncated": true});
            }
            let at = a.at.clone();
            let message_en = anomaly_message_en(&a.kind, &a.message, at.as_deref());
            let kind_label = kind_label(&a.kind);
            out.push(json!({
                "date": date,
                "kind": a.kind,
                "kind_label": kind_label,
                "severity": a.severity,
                "message": a.message,
                "message_en": message_en,
                "at": at,
            }));
        }
    }
    json!({"anomalies": out, "truncated": false})
}

/// GET /api/status — 今日日期 + 最新事件时间戳（采集器存活的保守代理）。
/// db_path 只返回文件名（审查 P2：全路径暴露安装目录/用户名等本机拓扑）；
/// 另带 db_dir_kind 提示数据目录性质（exe 同目录 / 其他），不暴露具体路径。
pub fn api_status(conn: &Connection, db_path: &Path) -> Value {
    // 自动更新检查结果（托盘每日检查线程写 data\update-available.txt）：
    // 有新版本时面板状态栏同步提示，与托盘菜单的一键更新项互为入口。
    let update_available = db_path
        .parent()
        .and_then(|d| std::fs::read_to_string(d.join("update-available.txt")).ok())
        .map(|s| s.trim().trim_start_matches('v').to_string())
        .filter(|v| {
            // 长度/字符集双限（文件可被任意本地进程写）+ 与当前版本比较
            //（一致性审查：手动装完新版后文件要等下次检查才刷新，不比较会
            // 挂着过期横幅）
            v.len() <= 16
                && v.len() >= 3
                && v.chars().all(|c| c.is_ascii_digit() || c == '.')
                && version_gt(v, env!("CARGO_PKG_VERSION"))
        });
    json!({
        "today": queries::today_local_str(),
        "last_event_ts": queries::latest_event_ts(conn),
        "db_path": db_path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default(),
        "db_dir_kind": db_dir_kind(db_path),
        "bind": "127.0.0.1",
        "read_only": true,
        "update_available": update_available,
    })
}

/// 语义化版本比较 a > b（数字三段式；与 tray 侧同口径）。
fn version_gt(a: &str, b: &str) -> bool {
    let parse = |v: &str| -> Vec<u64> {
        v.split('-')
            .next()
            .unwrap_or(v)
            .split('.')
            .map(|p| p.parse().unwrap_or(0))
            .collect()
    };
    let (pa, pb) = (parse(a), parse(b));
    for i in 0..3 {
        let x = pa.get(i).copied().unwrap_or(0);
        let y = pb.get(i).copied().unwrap_or(0);
        if x != y {
            return x > y;
        }
    }
    false
}

/// 数据目录性质提示（审查 P2：只给类别，不给路径）。db 与当前 exe 同目录时
/// 记 "exe-relative data"（安装版/便携版典型布局），否则记 "user data"。
fn db_dir_kind(db_path: &Path) -> &'static str {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()));
    match (db_path.parent(), exe_dir) {
        (Some(db), Some(exe)) if db == exe => "exe-relative data",
        _ => "user data",
    }
}

// bridge_count 已下沉到 kynoptic-core（queries::bridge_count），dash/cli 共用。

pub fn api_overview(conn: &Connection, db_path: &Path) -> Value {
    let today = queries::today_local_str();
    let (start, end) = queries::today_range();
    let today_events = queries::count_today_events(conn, &start, &end);
    let s = settings::load(db_path);

    let (session_started_at, session_open, uptime_seconds) = match conn
        .query_row(
            "SELECT start_time, end_time FROM sessions ORDER BY id DESC LIMIT 1",
            [],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)),
        )
        .ok()
    {
        Some((st, end)) => {
            let uptime = chrono::DateTime::parse_from_rfc3339(&st)
                .ok()
                .map(|t| (Utc::now() - t.with_timezone(&Utc)).num_seconds().max(0));
            (
                Value::from(st),
                Value::from(end.is_none()),
                uptime.map(Value::from).unwrap_or(Value::Null),
            )
        }
        None => (Value::Null, Value::Null, Value::Null),
    };

    // DB 大小：主文件 + WAL（metadata，不碰内容）
    let db_size_bytes = std::fs::metadata(db_path).map(|m| m.len()).unwrap_or(0);
    let db_wal_bytes = std::fs::metadata(db_path_with_wal(db_path))
        .map(|m| m.len())
        .unwrap_or(0);

    // 系统信息：与 MCP get_current_status 同一实现（cpu/mem/前台应用）
    let sys = kynoptic_mcp::state::current_status(conn, None).unwrap_or_else(|_| json!({}));

    // 硬件身份与容量：型号/CPU/GPU（注册表）+ 内存总量/磁盘（最近设备快照）
    let snap = latest_device_snapshot(conn);
    let host = {
        let mut h = host_identity();
        if let Some(mem) = snap.get("memory") {
            h["mem_total_gb"] = mem.get("total_gb").cloned().unwrap_or(json!(null));
            h["mem_used_gb"] = json!(mem
                .get("total_gb")
                .and_then(|v| v.as_f64())
                .zip(mem.get("available_gb").and_then(|v| v.as_f64()))
                .map(|(t, a)| (t - a) * 10.0 / 10.0));
        }
        h["disks"] = snap.get("disks").cloned().unwrap_or(json!([]));
        h["gpu_usage_pct"] = match gpu_usage_pct() {
            Some(v) => json!(v),
            None => json!(null),
        };
        h
    };

    // 三指标模型：权威口径已下沉到 kynoptic-core（queries::classify_minutes，
    // 单一实现；判定 human = keys-injected_keys + clicks-injected_clicks > 0，
    // auto = injected_keys + injected_clicks > 0，混合分钟双计，桥接读 settings）。
    // 窗口切换不计时长（可能是自动化开窗），只用于前台应用归类。
    let presence_day = queries::classify_minutes(conn, &today, s.presence_bridge_minutes);
    let presence_minutes = presence_day.presence_minutes;
    let automation_minutes = presence_day.automation_minutes;
    let mixed_minutes = presence_day.mixed_minutes;
    // 对比上下文（定性审查：Overview 数字无比较像裸报表）：昨日同口径三指标
    let yesterday = queries::date_offset_str(-1);
    let yday = queries::classify_minutes(conn, &yesterday, s.presence_bridge_minutes);
    let first_presence = presence_day
        .first_activity
        .map(Value::from)
        .unwrap_or(Value::Null);
    let last_presence = presence_day
        .last_activity
        .map(Value::from)
        .unwrap_or(Value::Null);

    // 今日前台应用时长（窗口切换间隔推算；不封顶：连续 N 小时就是 N 小时）
    let mut fg_dwell: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
    if let Ok(mut stmt) = conn.prepare(
        "SELECT timestamp, COALESCE(NULLIF(app_name,''), NULLIF(window_title,''), '(unknown)') FROM events          WHERE event_type = 'window' AND event_action = 'switch'            AND timestamp >= ?1 AND timestamp < ?2 ORDER BY timestamp",
    ) {
        let rows: Vec<(String, String)> = stmt
            .query_map(params![&start, &end], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })
            .map(|v| v.flatten().collect())
            .unwrap_or_default();
        let parse = |t: &str| chrono::DateTime::parse_from_rfc3339(t).ok();
        for pair in rows.windows(2) {
            if let (Some(a), Some(b)) = (parse(&pair[0].0), parse(&pair[1].0)) {
                let secs = (b - a).num_seconds(); // 不封顶：连续 N 小时就是 N 小时（审查 DeepSeek）
                // 采集停摆（暂停/看门狗杀/关机）产生的窗口间隔不能记成前台
                // 时长（全库审查 P1：8 小时关机会变成某应用 8 小时驻留），
                // 超 2h 的间隔两侧都不归属。
                if secs > 2 * 3600 {
                    continue;
                }
                *fg_dwell.entry(pair[0].1.clone()).or_insert(0) += secs;
            }
        }
    }
    let fg_top = fg_dwell
        .iter()
        .max_by_key(|(_, v)| **v)
        .map(|(a, v)| (a.clone(), *v));
    let fg_total_min: i64 = fg_dwell.values().sum::<i64>() / 60;
    // "机器替人值班"指标：有前台窗口但无任何输入（含桥接）的分钟数。
    // 暂无分钟级前台采样，取保守近似：fg_dwell_min - (presence + automation
    // - mixed)（审查：混合分钟同时计入 presence 与 automation，直接相减会把
    // 它们扣两遍——减并集只扣一次），负值截 0（宁可低估不夸大）。口径随响应返回。
    let unattended_fg_minutes =
        fg_total_min.saturating_sub((presence_minutes + automation_minutes - mixed_minutes).max(0));

    // "机器值班"第一小时误导防线：数据不满一整天时，该指标只是"开机至今减
    // 去活跃分钟"，人在场不 typing 也被累加。库中最早事件早于本地今日零点
    // 才视为"满一天"（timestamp 列是 UTC RFC3339，直接比较本地今日零点边界，
    // 不能截 UTC 日期字符串比较——UTC+8 会错位一天）。
    let earliest_ts: Option<String> = conn
        .query_row("SELECT MIN(timestamp) FROM events", [], |r| {
            r.get::<_, Option<String>>(0)
        })
        .ok()
        .flatten();
    let has_full_day = earliest_ts.map(|e| e < start).unwrap_or(false);

    json!({
        "host": host,
        "today": today,
        "today_events": today_events,
        "presence_minutes": presence_minutes,
        "automation_minutes": automation_minutes,
        "mixed_minutes": mixed_minutes,
        "presence_yesterday": yday.presence_minutes,
        "automation_yesterday": yday.automation_minutes,
        "metrics_note": "在场按你的真实键鼠和滚轮操作统计，自动化脚本的操作不计入在场，单独计为自动化。",
        "unattended_fg_minutes": unattended_fg_minutes,
        "unattended_fg_method": "保守近似：前台应用时长 −（在场 + 自动化 − 混合分钟），负值记 0",
        "has_full_day": has_full_day,
        "presence_bridge": s.presence_bridge_minutes.min(15),
        "fg_dwell_min": fg_total_min,
        "fg_top": fg_top.map(|(a, _)| Value::from(a)).unwrap_or(Value::Null),
        "first_activity": first_presence,
        "last_activity": last_presence,
        "monitors_enabled": s.enabled_monitors.len(),
        "monitors_total": registry::MONITOR_REGISTRY.len(),
        "input_counts_only": s.input_counts_only,
        "db_size_bytes": db_size_bytes,
        "db_wal_bytes": db_wal_bytes,
        "session_started_at": session_started_at,
        "session_open": session_open,
        "uptime_seconds": uptime_seconds,
        "cpu_pct": sys.get("cpu_pct").cloned().unwrap_or(Value::Null),
        "mem_pct": sys.get("mem_pct").cloned().unwrap_or(Value::Null),
        "foreground_app": sys.get("foreground_app").cloned().unwrap_or(Value::Null),
    })
}

fn db_path_with_wal(db_path: &Path) -> std::path::PathBuf {
    let mut s = db_path.as_os_str().to_os_string();
    s.push("-wal");
    std::path::PathBuf::from(s)
}

/// GET /api/heatmap?weeks= — 近 `weeks` 周按本地日聚合的活跃度。
/// 返回 `[{date, value}]`，按日升序，缺数天补零。`now` 注入以便测试。
///
/// 性能实证修复：旧实现扫 agg_minute 全史 GROUP BY（90 天库 cache miss
/// 实测 ~1s，overview 连续有输入天数/streak 卡即读此数据）。改读
/// daily_agg 日粒度派生缓存（date 为 PRIMARY KEY，一年最多 ~365 行，
/// PK 直取 ms 级）。语义不变：value = 当日本地时区活跃分钟口径
/// （keys/clicks，剔除纯移动；daily_agg.active_minutes 的口径见
/// core/src/daily_agg.rs）。
pub fn api_heatmap_at(conn: &Connection, weeks: u32, today: chrono::NaiveDate) -> Value {
    let weeks = weeks.clamp(1, 52);
    let days = i64::from(weeks) * 7;
    let since = today - chrono::Duration::days(days - 1);
    // 审查（DeepSeek）：口径 = "每日键鼠输入分钟"（绝对值）——事件条数会被
    // 系统事件与自动化注入通胀
    let mut by_date: std::collections::BTreeMap<String, i64> = {
        let mut m = std::collections::BTreeMap::new();
        if let Ok(mut stmt) =
            conn.prepare("SELECT date, active_minutes FROM daily_agg WHERE date >= ?1")
        {
            let since_str = since.format("%Y-%m-%d").to_string();
            if let Ok(rows) = stmt.query_map(params![&since_str], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
            }) {
                for (d, v) in rows.flatten() {
                    m.insert(d, v.max(0));
                }
            }
        }
        m
    };
    let mut out = Vec::new();
    let mut d = since;
    while d <= today {
        let key = d.format("%Y-%m-%d").to_string();
        let value = by_date.remove(&key).unwrap_or(0);
        out.push(json!({"date": key, "value": value}));
        d += chrono::Duration::days(1);
    }
    json!({"weeks": weeks, "days": out})
}

/// GET /api/apps?days= — 近 `days` 天（含今日）window 事件 Top 应用排行。
/// 应用名取 COALESCE(NULLIF(app_name,''), window_title)（采集器把可读名写进
/// window_title 而 app_name 常为空，与 MCP foreground_app 同一降敏约定），
/// 空名排除。`now` 注入以便测试。
pub fn api_apps_at(conn: &Connection, days: u32, today: chrono::NaiveDate) -> Value {
    let days = days.clamp(1, 365);
    let since_date = today - chrono::Duration::days(i64::from(days) - 1);
    let since = queries::local_day_range(&since_date.format("%Y-%m-%d").to_string())
        .map(|(s, _)| s)
        .unwrap_or_default();
    let stmt = conn
        .prepare(
            "SELECT COALESCE(NULLIF(app_name,''), window_title, '') AS app, COUNT(*) AS cnt \
             FROM events \
             WHERE event_type = 'window' AND timestamp >= ?1 AND app <> '' \
             GROUP BY app ORDER BY cnt DESC, app ASC LIMIT 10",
        )
        .map_err(|e| e.to_string());
    let apps: Vec<Value> = match stmt {
        Ok(mut s) => s
            .query_map(params![&since], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
            })
            .map_err(|e| e.to_string())
            .map(|rows| {
                rows.flatten()
                    .map(|(app, cnt)| json!({"app": app, "count": cnt}))
                    .collect::<Vec<Value>>()
            })
            .unwrap_or_default(),
        Err(_) => Vec::new(),
    };
    json!({"days": days, "apps": apps})
}

/// GET /api/hours?date= — 指定本地日 24 小时逐时活动量（事件总数），缺时补零。
pub fn api_hours(conn: &Connection, date: &str) -> std::result::Result<Value, String> {
    let date = match date {
        "" | "today" => queries::today_local_str(),
        d => d.to_string(),
    };
    let (start, end) =
        queries::local_day_range(&date).ok_or_else(|| date_err(&date, "YYYY-MM-DD 或 today"))?;
    let mut map = [0i64; 24];
    for (hour, cnt) in queries::hourly_counts_today(conn, &start, &end) {
        if (0..24).contains(&hour) {
            map[hour as usize] = cnt;
        }
    }
    Ok(json!({"date": date, "values": map}))
}

/// 硬件身份信息（型号 / CPU / GPU 名）。注册表直读、零子进程；
/// 值与进程同生命周期缓存（这些字段日常不变）。
fn host_identity() -> serde_json::Value {
    use std::sync::OnceLock;
    static CACHE: OnceLock<serde_json::Value> = OnceLock::new();
    CACHE
        .get_or_init(|| {
            use winreg::enums::HKEY_LOCAL_MACHINE;
            use winreg::RegKey;
            let rd = |path: &str, value: &str| -> String {
                RegKey::predef(HKEY_LOCAL_MACHINE)
                    .open_subkey(path)
                    .and_then(|k| k.get_value::<String, _>(value))
                    .unwrap_or_default()
            };
            let manufacturer = rd(r"HARDWARE\DESCRIPTION\System\BIOS", "SystemManufacturer");
            let product = rd(r"HARDWARE\DESCRIPTION\System\BIOS", "SystemProductName");
            let cpu = rd(
                r"HARDWARE\DESCRIPTION\System\CentralProcessor\0",
                "ProcessorNameString",
            );
            // GPU：显示适配器类驱动 0000-0009 的 DriverDesc。
            // 过滤远程桌面/虚拟屏适配器（IddDriver、Virtual Display 等干扰项）。
            let display_class = r"SYSTEM\CurrentControlSet\Control\Class\{4d36e968-e325-11ce-bfc1-08002be10318}";
            let mut gpus: Vec<String> = Vec::new();
            for idx in 0..10 {
                let desc = rd(&format!("{}{}{:04}", display_class, "\\", idx), "DriverDesc");
                let lower = desc.to_lowercase();
                let virtual_adapter = ["idd", "virtual display", "parity", "basic render"]
                    .iter()
                    .any(|v| lower.contains(v));
                if !desc.is_empty() && !virtual_adapter && !gpus.contains(&desc) {
                    gpus.push(desc);
                }
            }
            json!({
                "model": if manufacturer.is_empty() { product.clone() } else {
                    if product.is_empty() { manufacturer } else { format!("{manufacturer} {product}") }
                },
                "cpu": cpu,
                "gpus": gpus,
            })
        })
        .clone()
}

/// GPU 利用率（nvidia-smi 按需查询，3 秒缓存；不可用/非 N 卡返回 None）。
/// 这里主动打破"零子进程"的自我设限：用户要求的 GPU 占比只有这条正路，
/// 每次仅一个 ~50ms 的短查询且带缓存，代价可忽略。
fn gpu_usage_pct() -> Option<u64> {
    use std::sync::Mutex;
    use std::time::{Duration, Instant};
    static CACHE: Mutex<Option<(Instant, Option<u64>)>> = Mutex::new(None);
    const TTL: Duration = Duration::from_secs(3);
    // 缓存命中检查持锁，子进程绝不持锁（审查 P1：持锁跑无超时子进程，
    // nvidia-smi 卡死 = 面板整体死锁）。
    {
        let g = CACHE.lock().ok()?;
        if let Some((at, v)) = g.as_ref() {
            if at.elapsed() < TTL {
                return *v;
            }
        }
    }
    let out = command_output_capped(
        kynoptic_core::monitors::quiet_command("nvidia-smi").args([
            "--query-gpu=utilization.gpu",
            "--format=csv,noheader,nounits",
        ]),
        Duration::from_secs(3),
    )
    .filter(|(ok, _)| *ok)
    .and_then(|(_, s)| s.lines().next().and_then(|l| l.trim().parse::<u64>().ok()));
    let mut g = CACHE.lock().ok()?;
    *g = Some((Instant::now(), out));
    out
}

/// 带超时的子进程 Output（审查 P1：std 无 wait_timeout，手写轮询；
/// 超时 kill；输出量小，退出后读取不会堵管道缓冲）。
fn command_output_capped(
    cmd: &mut std::process::Command,
    timeout: std::time::Duration,
) -> Option<(bool, String)> {
    use std::io::Read;
    use std::process::Stdio;
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;
    let deadline = std::time::Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break Some(st),
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            _ => break None,
        }
    };
    let Some(status) = status else {
        let _ = child.kill();
        let _ = child.wait();
        return None;
    };
    let mut s = String::new();
    if let Some(mut o) = child.stdout.take() {
        let _ = o.read_to_string(&mut s);
    }
    Some((status.success(), s))
}

/// 最近一次 device_snapshot 的 memory + disks（总量/剩余，来自采集器快照）。
fn latest_device_snapshot(conn: &Connection) -> serde_json::Value {
    let Ok(data) = conn.query_row(
        // WHERE 必须与 0010 迁移部分索引 idx_events_action_id_hw 的谓词完全同形
        // 才能命中（故去掉冗余 json_valid；event_data 只写合法 JSON 或 NULL）。
        "SELECT event_data FROM events          WHERE event_action = 'device_snapshot'            AND json_extract(event_data, '$.memory.total_gb') IS NOT NULL          ORDER BY id DESC LIMIT 1",
        [],
        |r| r.get::<_, Option<String>>(0),
    ) else {
        return json!({});
    };
    serde_json::from_str(&data.unwrap_or_default()).unwrap_or_else(|_| json!({}))
}

/// GET /api/input?days= — 输入统计聚合（近 `days` 天，含今日）。
///
/// 数据源是 input_agg 分钟计数行（keyboard 行含 per-key 频次 `vk` map，
/// mouse 行含分键点击/滚轮/移动距离）。只读聚合，缺天补零。
/// `now` 注入以便测试。
pub fn api_input_at(
    conn: &Connection,
    days: u32,
    today: chrono::NaiveDate,
) -> std::result::Result<Value, String> {
    let days = days.clamp(1, 365);
    let since_date = today - chrono::Duration::days(i64::from(days) - 1);
    let since = queries::local_day_range(&since_date.format("%Y-%m-%d").to_string())
        .map(|(s, _)| s)
        .unwrap_or_default();
    let mut stmt = conn
        .prepare(
            "SELECT substr(datetime(timestamp, ?1), 1, 13) AS hour_bucket, event_type, event_data \
             FROM events \
             WHERE event_action = 'input_agg' AND timestamp >= ?2",
        )
        .map_err(|e| e.to_string())?;
    let off = {
        let secs = Local::now().offset().local_minus_utc() as i64;
        format!(
            "{}{} seconds",
            if secs >= 0 { "+" } else { "-" },
            secs.abs()
        )
    };
    let rows: Vec<(String, String, Option<String>)> = stmt
        .query_map(params![&off, &since], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })
        .map_err(|e| e.to_string())?
        .flatten()
        .collect();

    #[derive(Default)]
    struct Totals {
        keys: u64,
        clicks: u64,
        left: u64,
        right: u64,
        middle: u64,
        side1: u64,
        side2: u64,
        scroll_ticks: u64,
        moves: u64,
        dist_px: u64,
    }
    let mut totals = Totals::default();
    let mut key_freq: std::collections::BTreeMap<String, u64> = std::collections::BTreeMap::new();
    let mut series: std::collections::BTreeMap<String, (u64, u64)> =
        std::collections::BTreeMap::new();
    let today_prefix = today.format("%Y-%m-%d").to_string();
    let mut hourly_today: [u64; 24] = [0; 24];
    let minute_rows = rows.len();

    for (bucket, etype, data) in rows {
        let v: Value = match data.as_deref().and_then(|s| serde_json::from_str(s).ok()) {
            Some(v) => v,
            None => continue,
        };
        let num = |k: &str| v.get(k).and_then(|x| x.as_u64()).unwrap_or(0);
        let day: String = bucket.chars().take(10).collect();
        let e = series.entry(day).or_default();
        // 今日逐时输入量（keys+clicks），供"输入节奏"条形图
        if bucket.starts_with(&today_prefix) {
            if let Ok(h) = bucket.get(11..13).unwrap_or("").parse::<usize>() {
                if h < 24 {
                    if etype == "keyboard" {
                        hourly_today[h] += num("keys");
                    } else if etype == "mouse" {
                        hourly_today[h] += num("clicks");
                    }
                }
            }
        }
        if etype == "keyboard" {
            let keys = num("keys");
            totals.keys += keys;
            e.0 += keys;
            if let Some(map) = v.get("vk").and_then(|x| x.as_object()) {
                for (k, n) in map {
                    if let Some(n) = n.as_u64() {
                        *key_freq.entry(k.clone()).or_default() += n;
                    }
                }
            }
        } else if etype == "mouse" {
            let clicks = num("clicks");
            totals.clicks += clicks;
            totals.left += num("clicks_left");
            totals.right += num("clicks_right");
            totals.middle += num("clicks_middle");
            totals.side1 += num("clicks_side1");
            totals.side2 += num("clicks_side2");
            totals.scroll_ticks += num("scroll_ticks");
            totals.moves += num("moves");
            totals.dist_px += num("move_distance_px");
            e.1 += clicks;
        }
    }

    // 审查：raw 粒度（opt-in 逐键）库里没有 input_agg 计数行，此前本接口
    // 恒返回全 0。分钟行缺席时回退为 press/click 原始行聚合（keys = COUNT
    // press，clicks = COUNT click；per-key 图按 $.vk_code 分组），并在响应里
    // 标注 granularity 供前端区分口径。
    let mut granularity = "minute";
    if minute_rows == 0 {
        granularity = "raw";
        // 近 N 天每日 keys/clicks
        if let Ok(mut stmt) = conn.prepare(
            "SELECT substr(datetime(timestamp, ?1), 1, 10) AS d, event_type, COUNT(*) \
             FROM events \
             WHERE event_type IN ('keyboard','mouse') \
               AND event_action IN ('press','click') AND timestamp >= ?2 \
             GROUP BY d, event_type",
        ) {
            if let Ok(r) = stmt.query_map(params![&off, &since], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            }) {
                for (d, etype, cnt) in r.flatten() {
                    let cnt = cnt.max(0) as u64;
                    let e = series.entry(d).or_default();
                    if etype == "keyboard" {
                        totals.keys += cnt;
                        e.0 += cnt;
                    } else if etype == "mouse" {
                        totals.clicks += cnt;
                        e.1 += cnt;
                    }
                }
            }
        }
        // 今日逐时输入量（keys+clicks 口径与 minute 行一致）
        if let Some((tstart, tend)) = queries::local_day_range(&today_prefix) {
            if let Ok(mut stmt) = conn.prepare(
                "SELECT CAST(substr(datetime(timestamp, ?1), 12, 2) AS INTEGER) AS hh, \
                        event_type, COUNT(*) \
                 FROM events \
                 WHERE event_type IN ('keyboard','mouse') \
                   AND event_action IN ('press','click') \
                   AND timestamp >= ?2 AND timestamp < ?3 \
                 GROUP BY hh, event_type",
            ) {
                if let Ok(r) = stmt.query_map(params![&off, &tstart, &tend], |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, i64>(2)?,
                    ))
                }) {
                    for (hh, etype, cnt) in r.flatten() {
                        let h = hh as usize;
                        if h < 24 && (etype == "keyboard" || etype == "mouse") {
                            hourly_today[h] += cnt.max(0) as u64;
                        }
                    }
                }
            }
        }
        // per-key 键频（raw：press 行 $.vk_code）
        if let Ok(mut stmt) = conn.prepare(
            "SELECT json_extract(event_data, '$.vk_code') AS vk, COUNT(*) \
             FROM events \
             WHERE event_type = 'keyboard' AND event_action = 'press' \
               AND timestamp >= ?1 AND json_valid(event_data) \
               AND json_extract(event_data, '$.vk_code') IS NOT NULL \
             GROUP BY vk",
        ) {
            if let Ok(r) = stmt.query_map(params![&since], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
            }) {
                for (vk, cnt) in r.flatten() {
                    if (0..=255).contains(&vk) && cnt > 0 {
                        key_freq.insert(vk.to_string(), cnt as u64);
                    }
                }
            }
        }
    }

    let days_out: Vec<Value> = series
        .iter()
        .map(|(d, (keys, clicks))| json!({"date": d, "keys": keys, "clicks": clicks}))
        .collect();
    // 最近一次输入设备拓扑快照（device_snapshot.input_devices，仅拓扑变化时写入）
    let input_devices: Vec<Value> = conn
        .query_row(
            "SELECT event_data FROM events              WHERE event_action = 'device_snapshot' AND json_extract(event_data, '$.input_devices') IS NOT NULL              ORDER BY id DESC LIMIT 1",
            [],
            |r| r.get::<_, Option<String>>(0),
        )
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s?).ok())
        .and_then(|v| v.get("input_devices").cloned())
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default();
    Ok(json!({
        "days": days,
        "input_devices": input_devices,
        "granularity": granularity,
        "keys_total": totals.keys,
        "clicks_total": totals.clicks,
        "clicks_left": totals.left,
        "clicks_right": totals.right,
        "clicks_middle": totals.middle,
        "clicks_side1": totals.side1,
        "clicks_side2": totals.side2,
        "hourly_today": hourly_today,
        "scroll_ticks": totals.scroll_ticks,
        "moves": totals.moves,
        "move_distance_px": totals.dist_px,
        "key_freq": key_freq,
        "series": days_out,
    }))
}

/// GET /api/insights — 从原始数据挖掘叙事式发现（审查用户理念：
/// insights 缓存命中计数（api_insights / insights_cache_hits 共享）。
static INSIGHTS_CACHE_HITS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// 数据是矿石，洞察才是金子）。全部基于已有键鼠/窗口事件，零新增采集。
/// `bridge_minutes`：连续性间隙阈值（分钟），读 settings 的 presence_bridge，
/// 与 overview/report 的"连续性"口径一致。
///
/// 60 秒 TTL 缓存（审查 P2：单次全量重算 ~34ms，每次面板刷新都付）。
/// 结果与 `bridge_minutes` 一起缓存；命中时不触碰数据库，响应内容不变。
/// 命中计数经 [`insights_cache_hits`] 暴露，供测试与观测。
pub fn api_insights(conn: &Connection, bridge_minutes: u32) -> Value {
    use std::sync::Mutex;
    use std::time::{Duration, Instant};
    static CACHE: Mutex<Option<(Instant, u32, Value)>> = Mutex::new(None);
    const TTL: Duration = Duration::from_secs(60);
    {
        let g = CACHE.lock().ok();
        if let Some(c) = g.as_ref().and_then(|g| g.as_ref()) {
            if c.1 == bridge_minutes && c.0.elapsed() < TTL {
                INSIGHTS_CACHE_HITS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return c.2.clone();
            }
        }
    }
    let v = insights_compute(conn, bridge_minutes, chrono::Local::now());
    if let Ok(mut g) = CACHE.lock() {
        *g = Some((Instant::now(), bridge_minutes, v.clone()));
    }
    v
}

/// [`api_insights`] 的可注入时刻版本（绕过缓存）：`now` 指定"当前时刻"，
/// 供测试锚定日历、永不随真实日期漂移。
pub fn api_insights_at(
    conn: &Connection,
    bridge_minutes: u32,
    now: chrono::DateTime<chrono::Local>,
) -> Value {
    insights_compute(conn, bridge_minutes, now)
}

/// 测试/观测用：api_insights 缓存命中次数。
pub fn insights_cache_hits() -> u64 {
    INSIGHTS_CACHE_HITS.load(std::sync::atomic::Ordering::Relaxed)
}

/// insights 的实际计算（无缓存）。见 [`api_insights`]。
/// `now` 可注入，窗口与"今天"全部由它推导（测试锚定日历用）。
fn insights_compute(
    conn: &Connection,
    bridge_minutes: u32,
    now: chrono::DateTime<chrono::Local>,
) -> Value {
    let bridge_minutes = bridge_minutes.clamp(0, 15);
    let gap_secs = i64::from(bridge_minutes) * 60;
    // 窗口 = 6 天前零点 -> 现在（踩坑：local_day_range(date-6) 的 end 是
    // "6 天前当天"的结束，会让整个查询窗口落在有数据之前）
    let start_date = (now.date_naive() - chrono::Duration::days(6))
        .format("%Y-%m-%d")
        .to_string();
    let start = queries::local_day_range(&start_date)
        .map(|(s2, _)| s2)
        .ok_or_else(|| json!({"insights": []}));
    let start = match start {
        Ok(v) => v,
        Err(e) => return e,
    };
    let end = now.to_rfc3339();
    // 7 天窗口的人侧事件：timestamp、类型、应用
    let mut acts: Vec<(chrono::DateTime<chrono::FixedOffset>, String)> = Vec::new();
    let mut switches: Vec<(chrono::DateTime<chrono::FixedOffset>, String)> = Vec::new();
    if let Ok(mut stmt) = conn.prepare(
        // 审查 P2：专注 streak 与 marathon 同口径——剔除 move-only 事件，
        // 否则鼠标宏/自动晃动能"养"出假专注块（marathon 已挡，这里漏了）。
        // Wave20 P0：旧条件 `mouse AND action != 'move'` 在 minute 聚合模式下
        // 把每一行 input_agg（含纯移动、纯注入分钟）都当"一次输入"——
        // 最长专注/深夜/黄金/节律四张卡全部失真。人侧行级判定：
        // raw press/click 各算一条；input_agg 行仅当 keys/clicks 减注入后 > 0。
        "SELECT timestamp, event_type, event_action, COALESCE(NULLIF(app_name,''), '') FROM events          WHERE (event_type = 'window'             OR (event_type = 'keyboard' AND event_action = 'press')             OR (event_type = 'mouse' AND event_action = 'click')             OR (event_type IN ('keyboard','mouse') AND event_action = 'input_agg' AND json_valid(event_data)                 AND COALESCE(json_extract(event_data,'$.keys'),0) - COALESCE(json_extract(event_data,'$.injected_keys'),0)                   + COALESCE(json_extract(event_data,'$.clicks'),0) - COALESCE(json_extract(event_data,'$.injected_clicks'),0) > 0))            AND timestamp >= ?1 AND timestamp < ?2 ORDER BY timestamp",
    ) {
        if let Ok(rows) = stmt.query_map(params![&start, &end], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
            ))
        }) {
            for (ts, et, _action, app) in rows.flatten() {
                if let Ok(t) = chrono::DateTime::parse_from_rfc3339(&ts) {
                    if et == "window" {
                        switches.push((t, app));
                    } else {
                        acts.push((t, app));
                    }
                }
            }
        }
    }
    let mut insights: Vec<Value> = Vec::new();
    if acts.len() < 50 {
        // 文案实证修复：前端曾对空列表硬编码"采集满一天后…"，与真实门槛
        // （7 天窗口 >=50 条输入事件）不符。acts<50 但今日已有数据时，
        // 输出一张"数据积累中"的 info 卡替代空列表。
        let today_start =
            queries::local_day_range(&now.date_naive().format("%Y-%m-%d").to_string())
                .and_then(|(s2, _)| chrono::DateTime::parse_from_rfc3339(&s2).ok());
        let has_today = today_start
            .map(|s2| acts.iter().any(|(t, _)| *t >= s2))
            .unwrap_or(false);
        if has_today {
            let n = acts.len();
            return json!({"insights": [{
                "title_zh": "数据积累中",
                "title_en": "Still warming up",
                "text_zh": format!("已记录 {n} 条输入事件，积累到 50 条后生成洞察。"),
                "text_en": format!("Still warming up ({n} events so far) — insights appear after 50."),
            }]});
        }
        return json!({"insights": []});
    }
    let fmt_hm = |t: &chrono::DateTime<chrono::FixedOffset>| -> String {
        t.with_timezone(&chrono::Local).format("%H:%M").to_string()
    };
    let fmt_day = |t: chrono::DateTime<chrono::FixedOffset>| -> String {
        t.with_timezone(&chrono::Local)
            .format("%m-%d %H:%M")
            .to_string()
    };

    // 1) 最长连续在场段（输入间隔 <= presence_bridge 分钟算连续，口径与
    //    overview/report 一致）
    let mut best: (
        f64,
        Option<chrono::DateTime<chrono::FixedOffset>>,
        Option<chrono::DateTime<chrono::FixedOffset>>,
    ) = (0.0, None, None);
    let mut run_start: Option<chrono::DateTime<chrono::FixedOffset>> = None;
    let mut prev: Option<chrono::DateTime<chrono::FixedOffset>> = None;
    for (t, _) in &acts {
        if let (Some(p), Some(rs)) = (prev, run_start) {
            if (*t - p).num_seconds() > gap_secs {
                let run = (p - rs).num_seconds() as f64;
                if run > best.0 {
                    best = (run, Some(rs), Some(p));
                }
                run_start = Some(*t);
            }
        }
        if run_start.is_none() {
            run_start = Some(*t);
        }
        prev = Some(*t);
    }
    if let (Some(p), Some(rs)) = (prev, run_start) {
        let run = (p - rs).num_seconds() as f64;
        if run > best.0 {
            best = (run, Some(rs), Some(p));
        }
    }
    if best.0 > 600.0 {
        let (rs, re) = (
            best.1.map(fmt_day).unwrap_or_default(),
            best.2.map(fmt_day).unwrap_or_default(),
        );
        insights.push(json!({
            "title_zh": "最长连续在场（约）",
            "title_en": "Longest presence streak (approx.)",
            "text_zh": format!("近 7 天你最长的连续在场约 {:.0} 分钟（{} ~ {}），期间没有任何超过 {} 分钟的离开。窗口切换事件数口径，与异常页连续在场计数可能相差 ±1。", best.0 / 60.0, rs, re, bridge_minutes),
            "text_en": format!("Your longest continuous presence in the last 7 days was about {:.0} minutes ({} ~ {}) with no gap over {} minutes. Window-switch event counts may differ from the anomalies page by ±1.", best.0 / 60.0, rs, re, bridge_minutes),
        }));
    }

    // 2) 应用驻留 Top3（窗口切换间隔推算，单段上限 30 分钟）。
    //    P1 实证修复：注释声称"单段上限 30 分钟"但实现无封顶，跨天空档
    //    （如下一次切换在 3 天后）会输出 "appA 142.0h" 荒谬值。恢复封顶：
    //    单段贡献截断到 1800 秒。
    let mut dwell: std::collections::HashMap<String, f64> = std::collections::HashMap::new();
    for i in 0..switches.len() {
        let (t, a) = &switches[i];
        let nxt = switches.get(i + 1).map(|(t2, _)| *t2).unwrap_or(*t);
        let seg = (nxt - *t).num_seconds().clamp(0, 30 * 60);
        *dwell.entry(a.clone()).or_insert(0.0) += seg as f64;
    }
    let mut top: Vec<(String, f64)> = dwell.into_iter().collect();
    top.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    if top.len() >= 2 {
        let names: Vec<String> = top
            .iter()
            .take(3)
            .map(|(a, h)| format!("{} {:.1}h", a.trim_end_matches(".exe"), h / 3600.0))
            .collect();
        insights.push(json!({
            "title_zh": "前台应用时长 Top3",
            "title_en": "Top 3 apps by foreground time",
            "text_zh": format!("按窗口切换推算：{}", names.join("，")),
            "text_en": format!("Estimated from window switches: {}", names.join(", ")),
        }));
    }

    // 3) 上下文切换峰值时段
    let mut sw_hour: std::collections::HashMap<u32, usize> = std::collections::HashMap::new();
    for (t, _) in &switches {
        *sw_hour
            .entry(t.with_timezone(&chrono::Local).hour())
            .or_insert(0) += 1;
    }
    // 审查 P2：并列时取最早小时——HashMap 迭代无序，同一数据两次请求
    // 会给出不同的"最容易被打碎的时段"卡。
    if let Some((h, n)) = (0..=23u32)
        .filter_map(|h| sw_hour.get(&h).map(|n| (h, *n)))
        .max_by_key(|(_, n)| *n)
    {
        if n >= 20 {
            insights.push(json!({
                "title_zh": "最容易被打碎的时段",
                "title_en": "Most fragmented hour",
                "text_zh": format!("{:02}:00 前后是你切换窗口最频繁的时段（近 7 天 {} 次），注意力碎片化多发生在这里。", h, n),
                "text_en": format!("Around {:02}:00 you switch windows the most ({} times in 7 days) — that is where your focus fragments.", h, n),
            }));
        }
    }

    // 4) 深夜活动占比（口径与全站统一：[23:00, 次日 06:00)，含 23 点后
    //    与凌晨两段；文案注明窗口与单位）
    let night = acts
        .iter()
        .filter(|(t, _)| {
            let h = t.with_timezone(&chrono::Local).hour();
            !(6..23).contains(&h)
        })
        .count();
    if night > 30 {
        let pct = night as f64 / acts.len() as f64 * 100.0;
        insights.push(json!({
            "title_zh": "深夜活动",
            "title_en": "Late-night activity",
            "text_zh": format!("近 7 天有 {} 次键鼠输入发生在 23:00-06:00（含 23 点后与凌晨，占 {:.0}%）。", night, pct),
            "text_en": format!("{} keyboard/mouse events in the last 7 days happened between 23:00-06:00 (evening after 23:00 plus early morning, {:.0}%).", night, pct),
        }));
    }

    // 5) 黄金时段（输入最密集的连续 2 小时）。
    //    P3 实证修复：旧实现 `for h in 0..23` 漏掉 h=23，23:00-24:00 与
    //    0:00-1:00 的跨午夜组合永不当选——循环改 0..=23 取模配对。
    //    同时要求两小时各自事件数 >= 窗口总数的 25%：防止"空小时 + 密集
    //    小时"被拼成"最密集两小时"。
    let mut in_hour: std::collections::HashMap<u32, usize> = std::collections::HashMap::new();
    for (t, _) in &acts {
        *in_hour
            .entry(t.with_timezone(&chrono::Local).hour())
            .or_insert(0) += 1;
    }
    let in_total: usize = in_hour.values().sum();
    let mut best2: (usize, u32) = (0, 0);
    for h in 0..=23u32 {
        let a = in_hour.get(&h).copied().unwrap_or(0);
        let b = in_hour.get(&((h + 1) % 24)).copied().unwrap_or(0);
        if a * 4 < in_total || b * 4 < in_total {
            continue;
        }
        let sum = a + b;
        if sum > best2.0 {
            best2 = (sum, h);
        }
    }
    if best2.0 > 50 {
        insights.push(json!({
            "title_zh": "你的黄金时段",
            "title_en": "Your golden hours",
            "text_zh": format!("{:02}:00-{:02}:00 是你输入最密集的两小时——重要的活儿尽量放在这里。", best2.1, (best2.1 + 2) % 24),
            "text_en": format!("{:02}:00-{:02}:00 is your densest input window — schedule what matters here.", best2.1, (best2.1 + 2) % 24),
        }));
    }

    // 6) 节律（每天首次/最近在场，最多列 5 天）。
    //    修复跨日 bug：or_insert 只在首次插入，之前"最后输入"从未被更新，
    //    单事件日会显示"00:00~00:00"。现在每日末位持续更新；首末相同的
    //    日期（当天仅一条输入）明确标注，双语输出。
    let mut rhythm: std::collections::BTreeMap<String, (String, String)> =
        std::collections::BTreeMap::new();
    for (t, _) in &acts {
        let d = t.with_timezone(&chrono::Local).format("%m-%d").to_string();
        let hm = fmt_hm(t);
        let e = rhythm.entry(d).or_insert_with(|| (hm.clone(), hm.clone()));
        e.1 = hm;
    }
    if rhythm.len() >= 2 {
        let mut items_zh: Vec<String> = Vec::new();
        let mut items_en: Vec<String> = Vec::new();
        for (d, (f, l)) in rhythm.iter().rev().take(5) {
            if f == l {
                items_zh.push(format!("{d} 仅一条输入记录（{f}）"));
                items_en.push(format!("{d} single input event ({f})"));
            } else {
                items_zh.push(format!("{d} {f}~{l}"));
                items_en.push(format!("{d} {f}~{l}"));
            }
        }
        insights.push(json!({
            "title_zh": "你的作息节律",
            "title_en": "Your daily rhythm",
            "text_zh": format!("每天首次与最后输入：{}", items_zh.join("；")),
            "text_en": format!("First/last input per day: {}", items_en.join("; ")),
        }));
    }

    json!({"insights": insights, "dbg": {"acts": acts.len(), "switches": switches.len(), "built": insights.len()}})
}

/// GET /api/report?date= — 单日报告：色带时间轴、类别占比、专注时段。
///
/// dwell 分段：window/switch 事件间隔即上一应用的停留时长（不封顶）。
/// 专注块：间隔 <= presence_bridge 分钟（settings，默认 2，0-15）的连续活动且总长 >=20 分钟。
pub fn api_report_at(
    conn: &Connection,
    date: &str,
    s: &settings::AppSettings,
) -> std::result::Result<Value, String> {
    let date = match date {
        "" | "today" => queries::today_local_str(),
        d => d.to_string(),
    };
    let (start, end) =
        queries::local_day_range(&date).ok_or_else(|| date_err(&date, "YYYY-MM-DD 或 today"))?;
    let mut stmt = conn
        .prepare(
            "SELECT timestamp, COALESCE(NULLIF(app_name,''), NULLIF(window_title,''), '(unknown)') AS app, window_title              FROM events              WHERE event_type = 'window' AND event_action = 'switch'                AND timestamp >= ?1 AND timestamp < ?2              ORDER BY timestamp",
        )
        .map_err(|e| e.to_string())?;
    let rows: Vec<(String, String, Option<String>)> = stmt
        .query_map(params![&start, &end], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })
        .map_err(|e| e.to_string())?
        .flatten()
        .collect();
    let day_start = chrono::DateTime::parse_from_rfc3339(&start).map_err(|e| e.to_string())?;

    // 1) dwell 分段（分钟坐标）
    let mut segs: Vec<(String, String, usize, usize)> = Vec::new(); // (app, title, start_min, end_min)
    for (i, (ts, app, title)) in rows.iter().enumerate() {
        let Ok(t) = chrono::DateTime::parse_from_rfc3339(ts) else {
            continue;
        };
        let start_min = ((t - day_start).num_minutes().max(0) as usize).min(1439);
        let end_min = if i + 1 < rows.len() {
            match chrono::DateTime::parse_from_rfc3339(&rows[i + 1].0) {
                Ok(t2) => ((t2 - day_start).num_minutes().max(0) as usize).min(1440),
                Err(_) => start_min + 1,
            }
        } else {
            (start_min + 1).min(1440)
        };
        // 审查（DeepSeek 骂评）：不再 120 分钟封顶——连续同应用 3 小时就是
        // 3 小时前台时长，截断会把深度工作阉割掉；离场判定交给"人在场"指标
        if end_min <= start_min {
            continue;
        }
        // 同应用连续段合并
        if let Some(last) = segs.last_mut() {
            if last.0 == *app && start_min.saturating_sub(last.3) <= 1 {
                last.3 = end_min;
                continue;
            }
        }
        segs.push((
            app.clone(),
            title.clone().unwrap_or_default(),
            start_min,
            end_min,
        ));
    }

    // 2) 分类（Wave22 P1：title 此前恒传空串——依赖标题 token 的规则如
    // github/youtube 在 app_name 非空时永远够不着。现在双通道都喂给规则）
    let classify = |app: &str, title: &str| -> String {
        for rule in &s.categories {
            if rule.matches(app, title) {
                return rule.name.clone();
            }
        }
        "其他".to_string()
    };
    let segments: Vec<Value> = segs
        .iter()
        .map(|(app, title, a, b)| {
            json!({"app": app, "category": classify(app, title), "start_min": a, "end_min": b})
        })
        .collect();

    // 3) 类别占比（分钟）
    let mut cat_min: std::collections::BTreeMap<String, i64> = std::collections::BTreeMap::new();
    for (app, title, a, b) in &segs {
        *cat_min.entry(classify(app, title)).or_default() += (b - a) as i64;
    }
    let categories: Vec<Value> = cat_min
        .iter()
        .map(|(name, min)| json!({"category": name, "minutes": min}))
        .collect();

    // 4) 专注块：从"人在场分钟"出发（Wave20 P0：旧实现把 dwell 段间隙
    //    合并——dwell 段天然首尾相接，于是当天第一次到最后一次窗口切换
    //    全部连成一个"专注块"，离场/挂机时间全被算进去）。现在与 overview
    //    同一 classify 口径：人侧输入分钟为珠，间隙 <= bridge 分钟桥接，
    //    总长 >=20min 成块。
    let bridge_min = s.presence_bridge_minutes.min(15) as usize;
    let off = local_offset_modifier_at(chrono::Utc::now());
    let mut human_minutes: Vec<usize> = queries::minute_classification(conn, &start, &end, &off)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(bucket, human, _auto)| {
            if !human {
                return None;
            }
            chrono::NaiveDateTime::parse_from_str(&bucket, "%Y-%m-%d %H:%M")
                .ok()
                .map(|t| {
                    let ds = day_start.with_timezone(&chrono::Local).naive_local();
                    ((t - ds).num_minutes().max(0) as usize).min(1439)
                })
        })
        .collect();
    human_minutes.sort_unstable();
    human_minutes.dedup();
    let mut blocks: Vec<(usize, usize)> = Vec::new();
    for m in &human_minutes {
        if let Some(last) = blocks.last_mut() {
            if m.saturating_sub(last.1) <= bridge_min {
                last.1 = m + 1;
                continue;
            }
        }
        blocks.push((*m, m + 1));
    }
    let focus: Vec<Value> = blocks
        .iter()
        .filter(|(a, b)| b - a >= 20)
        .map(|(a, b)| json!({"start_min": a, "end_min": b, "minutes": b - a}))
        .collect();

    // 数据起始日（库中最早事件），供前端限制可选日期范围
    let data_since: Option<String> = conn
        .query_row(
            "SELECT MIN(substr(timestamp, 1, 10)) FROM events",
            [],
            |r| r.get::<_, Option<String>>(0),
        )
        .ok()
        .flatten();

    Ok(json!({
        "date": date,
        "segments": segments,
        "categories": categories,
        "focus": focus,
        "data_since": data_since,
    }))
}

/// GET /api/trends — 近 28 天每日 keys/clicks/active_minutes（来自 daily_agg 派生缓存）
/// 与 本 7 天 vs 上 7 天对比。
pub fn api_trends_at(conn: &Connection, today: chrono::NaiveDate) -> Value {
    let since_date = today - chrono::Duration::days(27);
    let since = since_date.format("%Y-%m-%d").to_string();
    let mut stmt = match conn.prepare(
        "SELECT date, keys, clicks, active_minutes FROM daily_agg WHERE date >= ?1 ORDER BY date",
    ) {
        Ok(s) => s,
        Err(_) => return json!({"daily": [], "this_week": {}, "last_week": {}}),
    };
    let rows: Vec<(String, i64, i64, i64)> = stmt
        .query_map(params![&since], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })
        .map(|rows| rows.flatten().collect())
        .unwrap_or_default();
    let daily: Vec<Value> = rows
        .iter()
        .map(|(d, k, c, m)| json!({"date": d, "keys": k, "clicks": c, "active_minutes": m}))
        .collect();
    // 口径（统一 2026-09）：sum7 按日历日对齐——窗口固定 7 个日历日
    // （today-offset-6 ..= today-offset），daily_agg 缺行（缺日）按 0 计。
    // 此前按行号切片 rows，缺日时本周窗口会"吃进"更早日期的行、两周对比错位。
    let mut by_date: std::collections::HashMap<&str, (i64, i64, i64)> =
        std::collections::HashMap::with_capacity(rows.len());
    for (d, k, c, m) in &rows {
        by_date.insert(d.as_str(), (*k, *c, *m));
    }
    let sum7 = |offset: i64| -> (i64, i64, i64) {
        (0..7)
            .map(|d| {
                let day = (today - chrono::Duration::days(offset + d))
                    .format("%Y-%m-%d")
                    .to_string();
                by_date.get(day.as_str()).copied().unwrap_or((0, 0, 0))
            })
            .fold((0, 0, 0), |a, b| (a.0 + b.0, a.1 + b.1, a.2 + b.2))
    };
    let (k1, c1, m1) = sum7(0);
    let (k0, c0, m0) = sum7(7);
    // 审查（DeepSeek）：周数据不足（<3 个有数据日）时对比无意义——本周与上周
    // 同一口径置空，前端显示"数据不足，已跳过对比"而非误导性汇总/增长率。
    // 本周此前无门槛：6 个缺日 + 1 天数据也会输出整周汇总，与 last_week 不对称。
    let week_days = |offset: i64| -> usize {
        (0..7)
            .filter(|d| {
                let day = (today - chrono::Duration::days(offset + i64::from(*d)))
                    .format("%Y-%m-%d")
                    .to_string();
                by_date.contains_key(day.as_str())
            })
            .count()
    };
    let (this_week_days, last_week_days) = (week_days(0), week_days(7));
    let this_week = if this_week_days >= 3 {
        json!({"keys": k1, "clicks": c1, "active_minutes": m1})
    } else {
        json!(null)
    };
    let last_week = if last_week_days >= 3 {
        json!({"keys": k0, "clicks": c0, "active_minutes": m0})
    } else {
        json!(null)
    };
    json!({
        "daily": daily,
        "this_week": this_week,
        "last_week": last_week,
        // 修复"人在场"假别名（第四口径）：曾经的 presence_minutes = active_minutes
        // 冒充在场（含注入、不桥接）。active_minutes 是 raw 输入口径（daily_agg
        // 派生缓存）；真正的"人在场"权威口径请看 /api/overview。
        "note": "active_minutes 为 raw 输入分钟口径（每日有键鼠输入的分钟数，来自 daily_agg 派生缓存，不剔注入、不桥接）；在场（human presence）请看 /api/overview / active_minutes is the raw input-minute metric (from the daily_agg cache, not injected-filtered, not bridged); for human presence see /api/overview",
    })
}

/// GET /api/apps_grid?date= — 指定本地日的"小时 × 应用"使用矩阵。
/// window 事件按本地小时桶 × 应用聚合，取当日 Top 6 应用 + 其余归并 other。
pub fn api_apps_grid_at(conn: &Connection, date: &str) -> std::result::Result<Value, String> {
    let date = match date {
        "" | "today" => queries::today_local_str(),
        d => d.to_string(),
    };
    let (start, end) =
        queries::local_day_range(&date).ok_or_else(|| date_err(&date, "YYYY-MM-DD 或 today"))?;
    let mut stmt = conn
        .prepare(
            "SELECT substr(datetime(timestamp, ?1), 12, 2) AS hh,                     COALESCE(NULLIF(app_name,''), NULLIF(window_title,''), '(unknown)') AS app,                     COUNT(*) AS cnt              FROM events              WHERE event_type = 'window' AND timestamp >= ?2 AND timestamp < ?3              GROUP BY hh, app",
        )
        .map_err(|e| e.to_string())?;
    let off = {
        let secs = Local::now().offset().local_minus_utc() as i64;
        format!(
            "{}{} seconds",
            if secs >= 0 { "+" } else { "-" },
            secs.abs()
        )
    };
    let rows: Vec<(String, String, i64)> = stmt
        .query_map(params![&off, &start, &end], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })
        .map_err(|e| e.to_string())?
        .flatten()
        .collect();

    let mut per_app: std::collections::BTreeMap<String, i64> = std::collections::BTreeMap::new();
    let mut by_hour_app: std::collections::BTreeMap<(String, String), i64> =
        std::collections::BTreeMap::new();
    for (hh, app, cnt) in rows {
        *per_app.entry(app.clone()).or_default() += cnt;
        *by_hour_app.entry((hh, app)).or_default() += cnt;
    }
    let mut ranked: Vec<(String, i64)> = per_app.into_iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let top: Vec<String> = ranked.iter().take(6).map(|(a, _)| a.clone()).collect();
    let mut grid: std::collections::BTreeMap<String, [i64; 24]> = std::collections::BTreeMap::new();
    for app in &top {
        grid.insert(app.clone(), [0; 24]);
    }
    grid.insert("(other)".into(), [0; 24]);
    let mut hourly_total = [0i64; 24];
    for ((hh, app), cnt) in &by_hour_app {
        if let Ok(h) = hh.parse::<usize>() {
            if h < 24 {
                hourly_total[h] += cnt;
                let key = if top.contains(app) {
                    app.as_str()
                } else {
                    "(other)"
                };
                if let Some(row) = grid.get_mut(key) {
                    row[h] += cnt;
                }
            }
        }
    }
    let grid_out: Vec<Value> = grid
        .into_iter()
        .map(|(app, hours)| json!({"app": app, "hours": hours}))
        .collect();
    let totals: Vec<Value> = ranked
        .iter()
        .take(6)
        .map(|(a, n)| json!({"app": a, "count": n}))
        .collect();
    Ok(json!({"date": date, "grid": grid_out, "totals": totals, "hourly_total": hourly_total}))
}

/// GET /api/daily_top?days= — 近 `days` 天每日 Top 3 应用（window 事件）。
pub fn api_daily_top_at(conn: &Connection, days: u32, today: chrono::NaiveDate) -> Value {
    let days = days.clamp(1, 90);
    let since_date = today - chrono::Duration::days(i64::from(days) - 1);
    let since = queries::local_day_range(&since_date.format("%Y-%m-%d").to_string())
        .map(|(s, _)| s)
        .unwrap_or_default();
    let mut stmt = match conn.prepare(
        "SELECT substr(datetime(timestamp, ?1), 1, 10) AS d,                 COALESCE(NULLIF(app_name,''), window_title, '') AS app, COUNT(*) AS cnt          FROM events          WHERE event_type = 'window' AND timestamp >= ?2 AND app <> ''          GROUP BY d, app",
    ) {
        Ok(s) => s,
        Err(_) => return json!({"days": days, "days_out": []}),
    };
    let off = {
        let secs = Local::now().offset().local_minus_utc() as i64;
        format!(
            "{}{} seconds",
            if secs >= 0 { "+" } else { "-" },
            secs.abs()
        )
    };
    let rows: Vec<(String, String, i64)> = stmt
        .query_map(params![&off, &since], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })
        .map(|rows| rows.flatten().collect())
        .unwrap_or_default();
    let mut by_day: std::collections::BTreeMap<String, Vec<(String, i64)>> =
        std::collections::BTreeMap::new();
    for (d, app, cnt) in rows {
        by_day.entry(d).or_default().push((app, cnt));
    }
    let days_out: Vec<Value> = by_day
        .into_iter()
        .map(|(d, mut apps)| {
            apps.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            apps.truncate(3);
            let top: Vec<Value> = apps
                .into_iter()
                .map(|(app, n)| json!({"app": app, "count": n}))
                .collect();
            json!({"date": d, "top": top})
        })
        .collect();
    json!({"days": days, "days_out": days_out})
}

/// GET /api/settings — 当前设置 + 全部监控器清单（来自 MONITOR_REGISTRY）。
pub fn api_settings(db_path: &Path) -> Value {
    let s = settings::load(db_path);
    settings_payload(&s)
}

/// 设置 + 监控器清单的统一响应体（GET 与 POST 共用）。
fn settings_payload(s: &AppSettings) -> Value {
    let monitors: Vec<Value> = registry::MONITOR_REGISTRY
        .iter()
        .map(|m| {
            json!({
                "id": m.id,
                "default_enabled": m.default_enabled,
                "sensitivity": m.sensitivity.as_str(),
            })
        })
        .collect();
    json!({
        "enabled_monitors": s.enabled_monitors,
        "autostart": s.autostart,
        "input_counts_only": s.input_counts_only,
        "dashboard_port": s.dashboard_port,
        "daily_goal_minutes": s.daily_goal_minutes,
        "presence_bridge_minutes": s.presence_bridge_minutes,
        "vk_frequency_enabled": s.vk_frequency_enabled,
        "categories": s.categories,
        "monitors": monitors,
    })
}

/// POST /api/settings — 接受 `enabled_monitors` / `autostart` / `dashboard_port`
/// / `input_counts_only` / `vk_frequency_enabled` 任一子集；监控器 id 必须全部在
/// 注册表内，否则 400。写盘后返回新设置；实际变更追加审计行到 settings-audit.log。
pub fn api_settings_post(db_path: &Path, body: &str) -> std::result::Result<Value, String> {
    let req: Value = serde_json::from_str(body).map_err(|e| format!("请求体不是合法 JSON: {e}"))?;
    // fuzz 加固：顶层数组/标量/null 在 get() 上全部落空 → 既往静默 200 并空
    // 转写 settings.json（顺带重排行尾）。显式 400，不留歧义。
    if !req.is_object() {
        return Err("请求体应为 JSON 对象".into());
    }
    let mut next = settings::load(db_path);
    let prev = next.clone();
    if let Some(v) = req.get("enabled_monitors") {
        let mut ids: Vec<String> = v
            .as_array()
            .ok_or("enabled_monitors 应为字符串数组")?
            .iter()
            .map(|x| {
                x.as_str()
                    .map(String::from)
                    .ok_or_else(|| "enabled_monitors 应为字符串数组".to_string())
            })
            .collect::<std::result::Result<_, _>>()?;
        if let Some(bad) = settings::first_invalid_id(&ids) {
            return Err(format!("未知监控器 id: {bad}"));
        }
        if ids.is_empty() {
            // Wave20 P1：空集 = 绿色"采集中"图标下的无声空采。要暂停请用
            // 托盘菜单 Pause（状态可见）。
            return Err("enabled_monitors 不能为空（暂停请用托盘菜单）".into());
        }
        // fuzz 加固：去重保序（重复 id 不改变语义，不必落库冗余条目）
        let mut seen = std::collections::HashSet::new();
        ids.retain(|id| seen.insert(id.clone()));
        next.enabled_monitors = ids;
    }
    if let Some(v) = req.get("input_counts_only") {
        next.input_counts_only = v.as_bool().ok_or("input_counts_only 应为布尔值")?;
    }
    if let Some(v) = req.get("vk_frequency_enabled") {
        next.vk_frequency_enabled = v.as_bool().ok_or("vk_frequency_enabled 应为布尔值")?;
    }
    if let Some(v) = req.get("autostart") {
        next.autostart = v.as_bool().ok_or("autostart 应为布尔值")?;
    }
    if let Some(v) = req.get("presence_bridge_minutes") {
        let m = v.as_u64().ok_or("presence_bridge_minutes 应为非负整数")?;
        if m > 15 {
            return Err("presence_bridge_minutes 不能超过 15".into());
        }
        next.presence_bridge_minutes = m as u32;
    }
    if let Some(v) = req.get("daily_goal_minutes") {
        let m = v.as_u64().ok_or("daily_goal_minutes 应为非负整数")?;
        if m > 24 * 60 {
            return Err("daily_goal_minutes 不能超过 1440".into());
        }
        next.daily_goal_minutes = m as u32;
    }
    if let Some(v) = req.get("categories") {
        let arr = v.as_array().ok_or("categories 应为数组")?;
        // fuzz 加固：条数与单条长度上限（拒绝毒值大对象落盘）
        if arr.len() > 100 {
            return Err(format!("categories 条数不能超过 100（当前 {}）", arr.len()));
        }
        let mut rules = Vec::with_capacity(arr.len());
        for r in arr {
            let name = r
                .get("name")
                .and_then(|x| x.as_str())
                .ok_or("categories[].name 缺失")?
                .to_string();
            let pattern = r
                .get("pattern")
                .and_then(|x| x.as_str())
                .ok_or("categories[].pattern 缺失")?
                .to_string();
            if name.len() > 256 {
                return Err("categories[].name 长度不能超过 256".into());
            }
            if pattern.len() > 256 {
                return Err("categories[].pattern 长度不能超过 256".into());
            }
            rules.push(settings::CategoryRule { name, pattern });
        }
        next.categories = rules;
    }
    if let Some(v) = req.get("dashboard_port") {
        let port = v.as_u64().ok_or("dashboard_port 应为 1-65535 整数")?;
        // fuzz 加固：0 也是毒值（绑定语义在 serve 层，落库 0 只会让面板打不开）
        if !(1..=u16::MAX as u64).contains(&port) {
            return Err("dashboard_port 应为 1-65535 整数".into());
        }
        next.dashboard_port = port as u16;
    }
    settings::save(db_path, &next).map_err(|e| format!("写设置失败: {e}"))?;
    if prev != next {
        append_settings_audit(db_path, &settings_audit_summary(&prev, &next));
    }
    SETTINGS_EPOCH.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut payload = settings_payload(&next);
    // 可选低成本兼容：未知顶层字段不 400，仅在响应里回 "ignored" 供前端排查
    const KNOWN: [&str; 9] = [
        "enabled_monitors",
        "input_counts_only",
        "vk_frequency_enabled",
        "autostart",
        "presence_bridge_minutes",
        "daily_goal_minutes",
        "categories",
        "dashboard_port",
        "monitors",
    ];
    let ignored: Vec<&str> = req
        .as_object()
        .map(|o| {
            o.keys()
                .filter(|k| !KNOWN.contains(&k.as_str()))
                .map(String::as_str)
                .collect()
        })
        .unwrap_or_default();
    if !ignored.is_empty() {
        payload["ignored"] = json!(ignored);
    }
    Ok(payload)
}

/// 单条审计记录的字节上限（时间戳 + 摘要 JSON 正常远小于此值）
const AUDIT_RECORD_CAP: usize = 512;

/// POST /api/settings 审计行：RFC3339 时间 + 变更字段摘要（JSON），一行一条。
/// 事件边界（任一字段被改）本身就是隐私敏感事件，必须留痕。
fn append_settings_audit(db_path: &Path, summary: &str) {
    let dir = match db_path.parent() {
        Some(d) if !d.as_os_str().is_empty() => d.to_path_buf(),
        _ => std::path::PathBuf::from("."),
    };
    let line = format!("{}\t{}\n", Utc::now().to_rfc3339(), summary);
    // 审查 P2：超限记录整条丢弃，不再硬截断——512 字节处下刀会把 JSON 记录
    // 劈成两半，上游任何逐行 JSON 解析都会在残行上炸掉。改为写一条占位记录
    // （{"truncated_record":true,"len":N}）保住"曾发生一次异常大的变更"这条
    // 审计线索，同时保证日志里每行都是完整 JSON。
    let line: String = if line.len() > AUDIT_RECORD_CAP {
        format!(
            "{}\t{{\"truncated_record\":true,\"len\":{}}}\n",
            Utc::now().to_rfc3339(),
            line.len().saturating_sub(1) // 去掉行尾换行计原始记录长度
        )
    } else {
        line
    };
    // 审计失败不影响主流程：设置已保存，日志尽力而为
    let target = dir.join("settings-audit.log");
    let result = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&target)
        .and_then(|mut f| std::io::Write::write_all(&mut f, line.as_bytes()));
    if let Err(e) = result {
        log::warn!("settings-audit.log 写入失败（不影响设置保存）: {e}");
    }
}

/// 变更摘要 JSON：只含被改字段的 旧值→新值；categories 不落全文（规则可能
/// 覆盖敏感应用名），只记条数变化。
fn settings_audit_summary(old: &AppSettings, new: &AppSettings) -> String {
    let mut d = serde_json::Map::new();
    if old.enabled_monitors != new.enabled_monitors {
        d.insert(
            "enabled_monitors".into(),
            json!([old.enabled_monitors.len(), new.enabled_monitors.len()]),
        );
    }
    for (k, a, b) in [
        ("autostart", old.autostart, new.autostart),
        (
            "input_counts_only",
            old.input_counts_only,
            new.input_counts_only,
        ),
        (
            "vk_frequency_enabled",
            old.vk_frequency_enabled,
            new.vk_frequency_enabled,
        ),
    ] {
        if a != b {
            d.insert(k.into(), json!([a, b]));
        }
    }
    if old.dashboard_port != new.dashboard_port {
        d.insert(
            "dashboard_port".into(),
            json!([old.dashboard_port, new.dashboard_port]),
        );
    }
    if old.daily_goal_minutes != new.daily_goal_minutes {
        d.insert(
            "daily_goal_minutes".into(),
            json!([old.daily_goal_minutes, new.daily_goal_minutes]),
        );
    }
    if old.presence_bridge_minutes != new.presence_bridge_minutes {
        d.insert(
            "presence_bridge_minutes".into(),
            json!([old.presence_bridge_minutes, new.presence_bridge_minutes]),
        );
    }
    if old.categories != new.categories {
        d.insert(
            "categories".into(),
            json!([old.categories.len(), new.categories.len()]),
        );
    }
    Value::Object(d).to_string()
}

// ─── 路由表 ─────────────────────────────────────────────────────────────────

/// 路由：返回 (HTTP status, content-type, body)。
/// `path` 含 query string；`body` 仅 POST /api/settings 使用；
/// 非 GET/POST → 405；未知路径 → 404；参数坏 → 400。
pub fn route_req(
    conn: &Connection,
    method: &str,
    path: &str,
    body: &str,
    db_path: &Path,
) -> (u16, &'static str, String) {
    let (route_path, query) = match path.split_once('?') {
        Some((p, q)) => (p, q),
        None => (path, ""),
    };
    let qval = |key: &str| -> Option<String> {
        query.split('&').find_map(|kv| {
            let (k, v) = kv.split_once('=')?;
            (k == key && !v.is_empty()).then(|| v.to_string())
        })
    };

    match (method, route_path) {
        ("GET", "/") => (200, "text/html; charset=utf-8", DASHBOARD_HTML.to_string()),
        ("GET", "/api/summary") => {
            let date = qval("date").unwrap_or_else(queries::today_local_str);
            if let Err(e) = reject_future_date(&date) {
                return (400, "application/json", err_json(&e));
            }
            match api_summary(conn, &date) {
                Ok(v) => (200, "application/json", v.to_string()),
                Err(e) => (400, "application/json", err_json(&e)),
            }
        }
        ("GET", "/api/timeline") => {
            // 负数会被 parse::<u32> 拒绝→回退 12（一致性：非法数值参数
            // 不静默放行，日期类参数都是 400——统一为 400）
            let hours = match qval("hours") {
                None => 12,
                Some(v) => match v.parse::<u32>() {
                    Ok(h) => h.clamp(1, 744),
                    Err(_) => {
                        return (
                            400,
                            "application/json",
                            err_json(&format!("hours 应为正整数（收到 {v:?}）")),
                        );
                    }
                },
            };
            // 桥接阈值读 settings（与 overview presence 同一口径源）
            let bridge = settings::load(db_path).presence_bridge_minutes.min(15);
            match api_timeline_at(conn, hours, Utc::now(), bridge) {
                Ok(v) => (200, "application/json", v.to_string()),
                Err(e) => (400, "application/json", err_json(&e)),
            }
        }
        ("GET", "/api/anomalies") => {
            let days = match qval("days") {
                None => 7,
                Some(v) => match v.parse::<u32>() {
                    Ok(d) => d,
                    Err(_) => return (400, "application/json", err_json("days 应为非负整数")),
                },
            };
            (
                200,
                "application/json",
                api_anomalies(conn, days, db_path).to_string(),
            )
        }
        ("GET", "/api/status") => (
            200,
            "application/json",
            api_status(conn, db_path).to_string(),
        ),
        ("GET", "/api/overview") => (
            200,
            "application/json",
            api_overview(conn, db_path).to_string(),
        ),
        ("GET", "/api/heatmap") => {
            let weeks = match qval("weeks") {
                None => 12,
                Some(v) => match v.parse::<u32>() {
                    Ok(w) => w.clamp(1, 52),
                    Err(_) => return (400, "application/json", err_json("weeks 应为非负整数")),
                },
            };
            (
                200,
                "application/json",
                api_heatmap_at(conn, weeks, today_naive()).to_string(),
            )
        }
        ("GET", "/api/apps") => {
            let days = match qval("days") {
                None => 7,
                Some(v) => match v.parse::<u32>() {
                    Ok(d) => d,
                    Err(_) => return (400, "application/json", err_json("days 应为非负整数")),
                },
            };
            (
                200,
                "application/json",
                api_apps_at(conn, days, today_naive()).to_string(),
            )
        }
        ("GET", "/api/hours") => {
            let date = qval("date").unwrap_or_else(|| "today".to_string());
            if let Err(e) = reject_future_date(&date) {
                return (400, "application/json", err_json(&e));
            }
            match api_hours(conn, &date) {
                Ok(v) => (200, "application/json", v.to_string()),
                Err(e) => (400, "application/json", err_json(&e)),
            }
        }
        ("GET", "/api/input") => {
            let days = match qval("days") {
                None => 7,
                Some(v) => match v.parse::<u32>() {
                    // 上限 90：input 端点把整段 input_agg 行全量拉进内存
                    //（Wave19：365 天 ≈ 52 万行 JSON，秒级响应+数百 MB 内存）
                    Ok(d) => d.clamp(1, 90),
                    Err(_) => return (400, "application/json", err_json("days 应为非负整数")),
                },
            };
            match api_input_at(conn, days, today_naive()) {
                Ok(v) => (200, "application/json", v.to_string()),
                Err(e) => (400, "application/json", err_json(&e)),
            }
        }
        ("GET", "/api/insights") => {
            // 连续性间隙阈值读 settings（presence_bridge，0-15 分钟），与
            // overview/report 的"连续性"口径全站一致
            let s = settings::load(db_path);
            let bridge = s.presence_bridge_minutes.min(15);
            (
                200,
                "application/json",
                api_insights(conn, bridge).to_string(),
            )
        }
        ("GET", "/api/report") => {
            let date = qval("date").unwrap_or_else(|| "today".to_string());
            if let Err(e) = reject_future_date(&date) {
                return (400, "application/json", err_json(&e));
            }
            let s = settings::load(db_path);
            match api_report_at(conn, &date, &s) {
                Ok(v) => (200, "application/json", v.to_string()),
                Err(e) => (400, "application/json", err_json(&e)),
            }
        }
        ("GET", "/api/trends") => (
            200,
            "application/json",
            api_trends_at(conn, today_naive()).to_string(),
        ),
        ("GET", "/api/apps_grid") => {
            let date = qval("date").unwrap_or_else(|| "today".to_string());
            if let Err(e) = reject_future_date(&date) {
                return (400, "application/json", err_json(&e));
            }
            match api_apps_grid_at(conn, &date) {
                Ok(v) => (200, "application/json", v.to_string()),
                Err(e) => (400, "application/json", err_json(&e)),
            }
        }
        ("GET", "/api/daily_top") => {
            let days = match qval("days") {
                None => 14,
                Some(v) => match v.parse::<u32>() {
                    Ok(d) => d,
                    Err(_) => return (400, "application/json", err_json("days 应为非负整数")),
                },
            };
            (
                200,
                "application/json",
                api_daily_top_at(conn, days, today_naive()).to_string(),
            )
        }
        ("GET", "/api/settings") => (200, "application/json", api_settings(db_path).to_string()),
        ("POST", "/api/settings") => {
            // 读-改-写整段串行化（审查 P1：并发 POST 会用旧快照覆盖对方字段）
            static SETTINGS_WRITE: std::sync::Mutex<()> = std::sync::Mutex::new(());
            let _guard = SETTINGS_WRITE.lock();
            match api_settings_post(db_path, body) {
                Ok(v) => (200, "application/json", v.to_string()),
                Err(e) => (400, "application/json", err_json(&e)),
            }
        }
        ("GET", _) => (404, "application/json", err_json("not found")),
        (_, _) => (405, "application/json", err_json("method not allowed")),
    }
}

/// 今日本地日期（NaiveDate 形，供 heatmap/apps 纯函数注入边界）。
fn today_naive() -> chrono::NaiveDate {
    Local::now().date_naive()
}

fn err_json(msg: &str) -> String {
    json!({"error": msg}).to_string()
}

/// 只读打开：优先走 MCP 工具面同款 `open_reader`（READ_ONLY + 全套 PRAGMA）。
/// 非 WAL 库上 `journal_mode=WAL` 会写入失败，此时回退为纯 READ_ONLY 连接
/// （仅设无副作用的 busy_timeout）——依然零写入、零迁移。READ_ONLY 连接对
/// WAL 库天然兼容（同进程采集器持写连接时多读一写并存）。
fn open_read_only(db_path: &Path) -> Result<Connection> {
    let path = db_path.to_string_lossy().to_string();
    match kynoptic_mcp::state::open_reader(&path) {
        Ok(conn) => Ok(conn),
        Err(_) => {
            let conn =
                Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                    .map_err(|e| {
                        Error::InvalidData(format!("无法只读打开 {}: {e}", db_path.display()))
                    })?;
            conn.execute_batch("PRAGMA busy_timeout=5000;")
                .map_err(|e| Error::InvalidData(e.to_string()))?;
            Ok(conn)
        }
    }
}

// ─── HTTP 服务（std::net 手写最小 handler） ─────────────────────────────────

/// 阻塞服务循环。仅绑定 127.0.0.1；每连接一线程内串行处理、响应后立即关闭。
///
/// `readonly=true`（cli 与 tray 均传 true）：DB 只读打开，唯一写路径是
/// POST /api/settings 写 settings.json；`false` 当前与 true 同策略，保留
/// 参数位以便未来显式放开（本 crate 现无任何 DB 写路径）。
/// 端口 0 = 随机空闲端口（实际端口经 log 输出）。
pub fn serve(db_path: &Path, port: u16, readonly: bool) -> Result<()> {
    let _ = readonly;
    // 预检 DB 可打开（保留原有报错路径）。常驻只读连接：冷缓存（页/索引加载）
    // 只付一次，之后请求复用（审查 P2：每请求新建连接冷 126ms vs 热 12ms）。
    // Connection 非 Sync，包 Mutex 跨线程共享；busy_timeout 2s——写侧是
    // 另一进程（tray 采集器），遇库锁最多等 2s。打开失败不致命：shared=None，
    // 每请求回退"临时新开只读连接"的旧路径保证可用性。
    let shared: Option<Arc<std::sync::Mutex<Connection>>> = {
        let _precheck = open_read_only(db_path)?;
        match open_read_only(db_path) {
            Ok(conn) => {
                let _ = conn.execute_batch("PRAGMA busy_timeout=2000;");
                Some(Arc::new(std::sync::Mutex::new(conn)))
            }
            Err(e) => {
                log::warn!("dashboard 常驻只读连接打开失败，回退每请求新开连接: {e}");
                None
            }
        }
    };
    #[allow(unused_variables)]
    let db_owned = db_path.to_path_buf();
    let listener = TcpListener::bind(("127.0.0.1", port)).map_err(|e| {
        Error::Io(std::io::Error::other(format!(
            "绑定 127.0.0.1:{port} 失败: {e}"
        )))
    })?;
    let bound = listener.local_addr().map(|a| a.port()).unwrap_or(port);
    log::info!(
        "dashboard: http://127.0.0.1:{bound}  (db: {}, read-only)",
        db_path.display()
    );
    println!(
        "dashboard: http://127.0.0.1:{bound}  (db: {}, read-only; Ctrl+C 停止)",
        db_path.display()
    );
    // 并发上限（审查 P2：无界 spawn + 每连接新建 SQLite 连接可被本地进程
    // 耗尽线程/句柄）。超限直接 503 拒绝，不排队。
    let inflight = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        if inflight.load(std::sync::atomic::Ordering::Relaxed) >= 64 {
            let mut s = stream;
            let _ = s.set_write_timeout(Some(std::time::Duration::from_secs(5)));
            let _ = http_simple(&mut s, 503, "text/plain", "too many connections");
            continue;
        }
        // 每连接一线程 + 5s 读写超时（审查 P0：旧实现单线程串行且无超时，
        // 一个半开连接/慢客户端就能挂死 accept 循环，整个面板假死）。
        // 复用常驻只读连接（Connection 非 Sync，经 Mutex 共享）。
        let shared_inner = shared.clone();
        let db_owned = db_owned.clone();
        let inflight_inner = inflight.clone();
        inflight.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        // 名额守卫：Drop 时归还。handle_client panic 或提前 return 都不会
        // 泄漏名额（审查 P2：原来 fetch_sub 在闭包尾部，panic 即泄漏，
        // 累计 64 次后面板永久 503）。
        struct InflightGuard<'a>(&'a std::sync::atomic::AtomicUsize);
        impl Drop for InflightGuard<'_> {
            fn drop(&mut self) {
                self.0.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
            }
        }
        // 名额只在闭包内归还（guard 语义：任何退出路径都 fetch_sub）。
        // 不可在 accept 循环里再建一个守卫——它在每轮迭代末 Drop，
        // 等于每个连接归还两次，计数下溢后永久 503。
        let spawned = std::thread::Builder::new()
            .name("dash-conn".into())
            .spawn(move || {
                let _guard = InflightGuard(&inflight_inner);
                // TCP_NODELAY（审查：偶发 5-20s 长尾候选因素——小 HTTP 响应
                // 遭 Nagle+延迟 ACK 相互等待）。
                let _ = stream.set_nodelay(true);
                let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(5)));
                let _ = stream.set_write_timeout(Some(std::time::Duration::from_secs(5)));
                if let Err(e) = handle_client(stream, shared_inner.as_ref(), &db_owned, bound) {
                    log::warn!("dashboard 连接处理失败: {e}");
                }
            });
        if spawned.is_err() {
            // spawn 失败：闭包从未运行，直接归还名额
            inflight.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        }
    }
    Ok(())
}

fn loopback_host_ok(host: &str, port: u16) -> bool {
    let h = host.trim();
    let h = h.strip_suffix('.').unwrap_or(h);
    matches!(h, "127.0.0.1" | "localhost")
        || h == format!("127.0.0.1:{port}")
        || h == format!("localhost:{port}")
}

/// 读请求行 → 校验 → route → 写响应。任何失败都静默断开（无日志面需求）。
///
/// `shared`：serve 预开的常驻只读连接（Mutex 共享，busy_timeout 2s）。
/// 锁不可用/中毒时回退本次请求临时新开只读连接，保证可用性。
fn handle_client(
    mut stream: TcpStream,
    shared: Option<&Arc<std::sync::Mutex<Connection>>>,
    db_path: &Path,
    port: u16,
) -> std::io::Result<()> {
    // 读请求行 + 头部（字节层解析；上限 8 KiB，超限 431，审查 P2）。
    let mut buf = [0u8; 4096];
    let mut raw = Vec::new();
    let header_end = loop {
        let n = stream.read(&mut buf)?;
        if n == 0 {
            break raw.len();
        }
        raw.extend_from_slice(&buf[..n]);
        if let Some(pos) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
        if raw.len() > 8192 {
            return http_simple(&mut stream, 431, "text/plain", "headers too large");
        }
    };
    let head = String::from_utf8_lossy(&raw[..header_end.min(raw.len())]).to_string();

    let (mut host, mut origin, mut fetch_site, mut marker, mut content_length) =
        (String::new(), None, None, false, 0usize);
    for l in head.lines().skip(1) {
        let Some((k, v)) = l.split_once(':') else {
            continue;
        };
        let v = v.trim().to_string();
        match k.trim().to_ascii_lowercase().as_str() {
            "host" => host = v,
            "origin" => origin = Some(v),
            "sec-fetch-site" => fetch_site = Some(v),
            "x-kynoptic" => marker = v == "1",
            "content-length" => content_length = v.parse().unwrap_or(0),
            _ => {}
        }
    }
    // DNS rebinding 防线：Host 必须是回环名。
    if !loopback_host_ok(&host, port) {
        return http_simple(&mut stream, 403, "text/plain", "forbidden host");
    }
    // CSRF 防线（审查 P0：无校验的 POST 可被任意网页打——最恶劣路径是静默
    // 开启逐键记录）。写请求必须带自定义头 X-Kynoptic: 1：跨站表单/simple
    // request 发不出自定义头；fetch 带它必触发 preflight，本服务不应答 CORS，
    // 攻击请求到不了这里。Origin/Sec-Fetch-Site 双保险。
    let is_post = head.lines().next().unwrap_or_default().starts_with("POST");
    if is_post {
        // 浏览器对 POST 请求恒发 Origin（同源 fetch 也带），要求必须存在且
        // 回环——不给"Origin 缺失放行"留口子（审查 P2：防线不依赖 marker 单点）。
        let origin_ok = origin
            .as_deref()
            .map(|o| {
                o == format!("http://127.0.0.1:{port}") || o == format!("http://localhost:{port}")
            })
            .unwrap_or(false);
        if !marker || !origin_ok || fetch_site.as_deref() == Some("cross-site") {
            return http_simple(
                &mut stream,
                403,
                "application/json",
                "{\"error\":\"cross-origin write blocked\"}",
            );
        }
    }
    // fuzz 加固：超限 body 直接 413，不再静默截断解析（旧路径截断到 64KB 后
    // 仍进 JSON 解析，且与客户端期望的字节数不一致会导致连接重置）。
    if content_length > 64 * 1024 {
        return http_simple(
            &mut stream,
            413,
            "application/json",
            "{\"error\":\"payload too large (max 65536 bytes)\"}",
        );
    }
    while raw.len() < header_end + content_length {
        let n = stream.read(&mut buf)?;
        if n == 0 {
            break;
        }
        raw.extend_from_slice(&buf[..n]);
    }
    // body 按字节层精确切分（审查 P2：lossy 会错位；pipelined 多发尾巴裁掉）。
    let body_end = (header_end + content_length).min(raw.len());
    let body = String::from_utf8_lossy(&raw[header_end.min(body_end)..body_end]).to_string();
    let request_line = head.lines().next().unwrap_or_default().to_string();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let path = parts.next().unwrap_or("/").to_string();

    // 优先复用常驻只读连接；拿不到锁或未预开时回退临时连接（旧路径）。
    let shared_guard = shared.and_then(|c| c.lock().ok());
    let tmp_conn;
    let conn: &Connection = match shared_guard.as_ref() {
        Some(g) => g,
        None => {
            tmp_conn = open_read_only(db_path).map_err(|e| std::io::Error::other(e.to_string()))?;
            &tmp_conn
        }
    };
    let (status, ctype, body) = route_req(conn, &method, &path, &body, db_path);
    http_simple(&mut stream, status, ctype, &body)
}

/// 统一安全响应头（审查 P1：所有响应必带）。CSP 按 dashboard.html 现状收窄：
/// 单文件内联 script/style（'unsafe-inline'），favicon 为 data: URI，无外链资源。
const SECURITY_HEADERS: &str = "X-Content-Type-Options: nosniff\r\n\
X-Frame-Options: DENY\r\n\
Cache-Control: no-store\r\n\
Content-Security-Policy: default-src 'self'; style-src 'unsafe-inline'; script-src 'unsafe-inline'; img-src 'self' data:\r\n";

fn http_simple(
    stream: &mut TcpStream,
    status: u16,
    ctype: &str,
    body: &str,
) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        413 => "Payload Too Large",
        431 => "Request Header Fields Too Large",
        503 => "Service Unavailable",
        _ => "Error",
    };
    write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {ctype}\r\n{SECURITY_HEADERS}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

#[cfg(test)]
mod insights_cache_test {
    use super::*;

    /// api_insights 60 秒 TTL 缓存：第二次调用命中缓存（计数 +1）且结果一致。
    /// bridge_minutes 用 3/4，避开 tests.rs 用例的 2，防止共享缓存串台。
    #[test]
    fn insights_cache_hits_on_second_call() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(kynoptic_core::db::SCHEMA).unwrap();
        let _ = kynoptic_core::db::run_migrations(&conn);
        for _ in 0..51 {
            conn.execute(
                "INSERT INTO events (timestamp, event_type, event_action, event_data, app_name, window_title, session_id) VALUES (?1,'keyboard','press',NULL,NULL,NULL,NULL)",
                params!["2026-06-15T09:00:00+00:00"],
            )
            .unwrap();
        }
        let before = insights_cache_hits();
        let v1 = api_insights(&conn, 3);
        let v2 = api_insights(&conn, 3);
        assert_eq!(v1, v2, "两次调用结果应一致");
        assert_eq!(insights_cache_hits(), before + 1, "第二次调用应命中缓存");
        // bridge 变了则不命中旧缓存（重算，计数不变）
        let _ = api_insights(&conn, 4);
        assert_eq!(insights_cache_hits(), before + 1);
    }
}

#[cfg(test)]
mod tests;
