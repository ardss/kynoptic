//! 行为分析：专注段、碎片化、APM 趋势、工作节奏
//!
//! 本模块是**纯业务规则层**——不直接执行 SQL，所有数据由 [`crate::queries`]
//! 的读取 API 预先取出后传入。这样业务算法可脱离数据库单测。
//!
//! 仅 `analyze_day` / `find_focus_segments(conn, date)` 作为编排入口保留
//! `&Connection`（供 Tauri command / ctl CLI 调用），内部委托纯函数。

use crate::queries::{self, MinuteStat};
use crate::time::minute_diff;
use crate::Result;
use rusqlite::Connection;
use serde::Serialize;

/// 一段"专注"区间：>= 5 分钟连续键盘/鼠标活动，且中途窗口切换次数 < 阈值
#[derive(Debug, Serialize, Clone)]
pub struct FocusSegment {
    pub start: String,     // ISO8601
    pub end: String,       // ISO8601
    pub duration_min: i64, // 持续分钟数
    pub key_count: i64,
    pub click_count: i64,
    pub app_name: Option<String>,
    pub window_title: Option<String>,
}

/// 一天的"碎片化指数"：1 = 完全碎片，0 = 完全连续
#[derive(Debug, Serialize)]
pub struct FragmentationScore {
    pub date: String,
    pub fragmentation_index: f64, // 0..1
    pub active_minutes: i64,
    pub longest_streak_min: i64,
    pub total_breaks: i64,
}

/// 一段时间内的 APM 序列
#[derive(Debug, Serialize)]
pub struct ApmPoint {
    pub minute: String, // "2026-06-15T10:30"
    pub apm: f64,
    pub keys: i64,
    pub clicks: i64,
}

/// 一天的整体分析报告
#[derive(Debug, Serialize)]
pub struct DayAnalysis {
    pub date: String,
    pub total_keys: i64,
    pub total_clicks: i64,
    pub active_minutes: i64,
    pub apm_avg: f64,
    pub focus_segments: Vec<FocusSegment>,
    pub fragmentation: FragmentationScore,
    pub apm_series: Vec<ApmPoint>,
}

/// 阈值常量（从 constants 复用，集中管理魔法数字）
use crate::constants::{FOCUS_MAX_WINDOW_SWITCHES_PER_MIN, FOCUS_MIN_MINUTES};

// ─── 纯业务函数（无 SQL，可单测） ─────────────────────────────────────────────

/// 计算两 ISO 时间戳相差的分钟数（向上取整）。
/// 委托给 [`crate::time::minute_diff`]（用 chrono 精确解析）。
fn duration_min(start: &str, end: &str) -> i64 {
    minute_diff(start, end)
}

/// 找出一天的"专注段"。逻辑：扫描每分钟的 keys/clicks/switches，
/// 合并连续 ≥ [`FOCUS_MIN_MINUTES`] 分钟且平均切换数 ≤
/// [`FOCUS_MAX_WINDOW_SWITCHES_PER_MIN`] 的段。
///
/// **纯函数**：输入由 [`queries::minute_stats_by_date`] 预先取出。
/// `app_window` 回调用于为每段补全 app/window 标签（接收 start/end ISO，
/// 返回 `Option<(app, title)>`），由编排层注入实际的 [`queries::top_app_window_in_range`]。
pub fn focus_segments_from_stats<F>(minutes: &[MinuteStat], mut app_window: F) -> Vec<FocusSegment>
where
    F: FnMut(&str, &str) -> Option<(String, String)>,
{
    // 段内累加：start, last_minute, sum_keys, sum_clicks, sum_switches
    let mut segments: Vec<FocusSegment> = Vec::new();
    let mut cur_start: Option<String> = None;
    let mut cur_end: Option<String> = None;
    let mut cur_keys = 0i64;
    let mut cur_clicks = 0i64;
    let mut cur_switches = 0i64;

    // 显式函数：刷出当前段
    fn flush(
        segments: &mut Vec<FocusSegment>,
        start: &mut Option<String>,
        end: &mut Option<String>,
        keys: &mut i64,
        clicks: &mut i64,
        switches: &mut i64,
    ) {
        if let (Some(s), Some(e)) = (start.take(), end.take()) {
            // 段长 = end_minute - start_minute + 1（包含两端）
            // 例如 10:00-10:04 → 5 个 distinct minutes
            let dur = duration_min(&s, &e) + 1;
            if dur >= FOCUS_MIN_MINUTES {
                let avg_switches = if dur > 0 { *switches / dur } else { 0 };
                if avg_switches <= FOCUS_MAX_WINDOW_SWITCHES_PER_MIN {
                    segments.push(FocusSegment {
                        start: s,
                        end: e,
                        duration_min: dur,
                        key_count: *keys,
                        click_count: *clicks,
                        app_name: None,
                        window_title: None,
                    });
                }
            }
        }
        *keys = 0;
        *clicks = 0;
        *switches = 0;
    }

    for (i, m) in minutes.iter().enumerate() {
        let is_active = (m.keys + m.clicks) > 0; // 该分钟有输入
        if is_active {
            let prev = if i > 0 {
                Some(&minutes[i - 1].minute)
            } else {
                None
            };
            let continuous = prev
                .map(|p| duration_min(p, &m.minute) == 1)
                .unwrap_or(false);

            if cur_start.is_none() {
                cur_start = Some(m.minute.clone());
                cur_keys = m.keys;
                cur_clicks = m.clicks;
                cur_switches = m.switches;
                cur_end = Some(m.minute.clone());
            } else if continuous {
                cur_keys += m.keys;
                cur_clicks += m.clicks;
                cur_switches += m.switches;
                cur_end = Some(m.minute.clone());
            } else {
                // 断开，flush
                flush(
                    &mut segments,
                    &mut cur_start,
                    &mut cur_end,
                    &mut cur_keys,
                    &mut cur_clicks,
                    &mut cur_switches,
                );
                cur_start = Some(m.minute.clone());
                cur_keys = m.keys;
                cur_clicks = m.clicks;
                cur_switches = m.switches;
                cur_end = Some(m.minute.clone());
            }
        } else {
            // 空闲分钟，flush
            flush(
                &mut segments,
                &mut cur_start,
                &mut cur_end,
                &mut cur_keys,
                &mut cur_clicks,
                &mut cur_switches,
            );
        }
    }
    // 收尾
    flush(
        &mut segments,
        &mut cur_start,
        &mut cur_end,
        &mut cur_keys,
        &mut cur_clicks,
        &mut cur_switches,
    );

    // 为每段补 app_name / window_title（取该段时间内最频繁的）
    for seg in &mut segments {
        if let Some((app, title)) = app_window(&seg.start, &seg.end) {
            seg.app_name = Some(app);
            seg.window_title = Some(title);
        }
    }

    segments
}

/// 计算某天的"碎片化指数"。
///
/// **纯函数**：输入是当日 distinct 活跃分钟列表（由
/// [`queries::active_minutes_by_date`] 取出）。复用
/// [`queries::longest_active_streak`] 算最长连续段。
pub fn fragmentation_from_minutes(active_minutes: &[String], date: &str) -> FragmentationScore {
    let count = active_minutes.len() as i64;
    if count == 0 {
        return FragmentationScore {
            date: date.to_string(),
            fragmentation_index: 0.0,
            active_minutes: 0,
            longest_streak_min: 0,
            total_breaks: 0,
        };
    }

    let (longest, breaks) = queries::longest_active_streak(active_minutes);

    // 碎片化指数：1 - (最长段 / 活跃总分钟)
    // 1.0 = 完全碎片（每个活跃分钟都孤立），0.0 = 完全连续
    let idx = 1.0 - (longest as f64 / count as f64);
    let idx = idx.clamp(0.0, 1.0);

    FragmentationScore {
        date: date.to_string(),
        fragmentation_index: (idx * 1000.0).round() / 1000.0,
        active_minutes: count,
        longest_streak_min: longest,
        total_breaks: breaks,
    }
}

/// 当天 APM 序列（每分钟）。
///
/// **纯函数**：输入是 [`queries::minute_stats_by_date`] 返回的每分钟统计。
/// 只保留 keys/clicks 至少有一个 > 0 的分钟。
pub fn apm_series_from_stats(minutes: &[MinuteStat]) -> Vec<ApmPoint> {
    minutes
        .iter()
        .filter(|m| m.keys > 0 || m.clicks > 0)
        .map(|m| {
            let keys = m.keys;
            let clicks = m.clicks;
            ApmPoint {
                minute: m.minute.clone(),
                apm: (keys + clicks) as f64,
                keys,
                clicks,
            }
        })
        .collect()
}

// ─── 编排层（保留 &Connection，供 commands/ctl 调用） ────────────────────────

/// 找出一天的"专注段"（编排入口）。
///
/// 读 [`queries::minute_stats_by_date`] + [`queries::top_app_window_in_range`]，
/// 委托纯函数 [`focus_segments_from_stats`]。Tauri command / ctl CLI 走这条路径。
pub fn find_focus_segments(conn: &Connection, date: &str) -> Result<Vec<FocusSegment>> {
    let minutes = queries::minute_stats_by_date(conn, date);
    Ok(focus_segments_from_stats(&minutes, |start, end| {
        queries::top_app_window_in_range(conn, start, end)
    }))
}

/// 整合：一天的完整分析（编排入口）。
///
/// 读 [`queries::day_totals`] / [`queries::minute_stats_by_date`] /
/// [`queries::active_minutes_by_date`]，委托纯函数组装 [`DayAnalysis`]。
pub fn analyze_day(conn: &Connection, date: &str) -> Result<DayAnalysis> {
    let totals = queries::day_totals(conn, date);
    let apm_avg = if totals.active_minutes > 0 {
        (totals.keys + totals.clicks) as f64 / totals.active_minutes as f64
    } else {
        0.0
    };

    let focus_segments = find_focus_segments(conn, date).unwrap_or_default();
    let active_minutes = queries::active_minutes_by_date(conn, date);
    let fragmentation = fragmentation_from_minutes(&active_minutes, date);
    let minute_stats = queries::minute_stats_by_date(conn, date);
    let apm_series = apm_series_from_stats(&minute_stats);

    Ok(DayAnalysis {
        date: date.to_string(),
        total_keys: totals.keys,
        total_clicks: totals.clicks,
        active_minutes: totals.active_minutes,
        apm_avg: (apm_avg * 10.0).round() / 10.0,
        focus_segments,
        fragmentation,
        apm_series,
    })
}
