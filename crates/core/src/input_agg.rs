//! 输入事件分钟聚合（opt-in `input_granularity: "minute"`）
//!
//! **默认是 raw**：Hook 回调与旧行为完全一致（逐事件入队落库）。仅在
//! [`crate::collector::CollectorSettings::input_granularity`] 设为 [`InputGranularity::Minute`]
//! 时，Hook 回调退化为纯原子计数（本模块 `record_*`），由采集器的聚合线程每秒
//! drain 一次、按**本地分钟桶**折叠成每桶一行的 `input_agg` 计数型事件：
//! - keyboard 行：`{"keys": K, "samples": S, "final": F}`
//! - mouse 行：`{"clicks": C, "scroll_ticks": T, "moves": M, "move_distance_px": D, "samples": S, "final": F}`
//!
//! `final`（审查 P1）：true = 该分钟已完整的终值行（由后续 drain 折叠）；
//! false = 秒级快照/关停部分行。重启时据此判定"抑制"还是"合并续写"
//! （见 [`clear_restart_suppression`]）。
//!
//! 下游计数查询（queries::*）对两种形态统一兼容（见 queries::KEYS_ROW_EXPR）。
//! 计数语义保持：APM、活跃分钟、daily_agg 等只依赖计数，不受粒度影响；
//! 按键明细/鼠标坐标热力图在 minute 粒度下无原始样本，属已知取舍。
//!
//! 原始数据神圣性：minute 模式只是**生成侧**的折叠，不删除、不改写任何已落库
//! 数据；切回 raw 即恢复逐事件存储。

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;

use chrono::{DateTime, Local, Timelike, Utc};

use crate::types::{Event, EventAction, EventType};

// ─── Hook 回调侧（每事件只有原子操作，无锁无分配） ─────────────────────────────

static MINUTE_MODE: AtomicBool = AtomicBool::new(false);

/// per-key（VK）键频记录开关（默认开启：本地数据完整优先）。
///
/// 默认 true：record_key_vk 同时累计 per-key 原子表（WhatPulse 式键盘热力图
/// 数据源）。只存"每个键按了多少次"，不存内容、顺序、时间戳。
/// 对隐私敏感的用户可经 [`crate::collector::CollectorSettings::vk_frequency_enabled`]
/// 显式关闭（tray/CLI 接线由对应侧完成）——是 opt-out，不是 opt-in。
static VK_ENABLED: AtomicBool = AtomicBool::new(true);

/// 开/关 per-key 键频记录。采集器启动时按设置调用一次；
/// 运行中切换也安全（关停即刻停止累计，已累计值会在下轮 drain 清零）。
pub fn set_vk_enabled(on: bool) {
    VK_ENABLED.store(on, Ordering::Relaxed);
}

/// 当前是否启用 per-key 键频记录。
pub fn vk_enabled() -> bool {
    VK_ENABLED.load(Ordering::Relaxed)
}

/// 当前是否处于 minute 聚合模式（Hook 回调每次调用读取，须保持廉价）。
pub(crate) fn minute_mode() -> bool {
    MINUTE_MODE.load(Ordering::Relaxed)
}

fn set_minute_mode(on: bool) {
    MINUTE_MODE.store(on, Ordering::Relaxed);
}

static KEYS: AtomicU64 = AtomicU64::new(0);
/// 最近一次输入事件所在分钟（UTC 纪元分钟；0 = 无记录）。Hook 回调侧廉价
/// 维护（一次 SystemTime::now + 一次原子 store），供聚合线程把 drain 到的
/// 计数归入**事件时间分钟**而非 drain 执行分钟（审查：聚合线程停滞/换页
/// 卡顿跨过分钟边界时，旧实现把上一分钟的计数记进新分钟桶，分钟图偏移）。
static LAST_EVENT_EPOCH_MIN: AtomicU64 = AtomicU64::new(0);

fn note_event_epoch_now() {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    LAST_EVENT_EPOCH_MIN.store(secs / 60, Ordering::Relaxed);
}

/// 本会话已折叠为终值（final=true）并落库的最大 UTC 纪元分钟（时钟回拨防护，
/// 审查 HIGH）。from_epoch_min 只钳 future 分钟、对过去无单调性防护：回拨
/// 60-119 秒落回刚折叠完的那一分钟时，后到输入会折出同键 (timestamp,etype)
/// 的行，UPSERT 整行覆盖把那分钟的真实终值改掉（历史被改写）。终值落库点
/// （drain rollover）记下分钟，之后的折叠若目标分钟 <= 已终值分钟则丢弃。
/// 只做会话内防护（reset 清零——重启场景由重启抑制 + $.final 判定负责）。
static LAST_FINALIZED_MIN: AtomicU64 = AtomicU64::new(0);

/// 目标分钟是否仍允许折叠落库（true = 未被更晚的终值覆盖过）。
fn fold_allowed(key: &MinuteKey) -> bool {
    let m = key.start_local.with_timezone(&Utc).timestamp() / 60;
    (m as u64) > LAST_FINALIZED_MIN.load(Ordering::Relaxed)
}
static CLICKS: AtomicU64 = AtomicU64::new(0);
/// per-key 频次（vk code 0..255 各一个原子计数）。只存"每个键按了多少次"，
/// 不存内容、顺序、时间戳——与计数红线同口径（WhatPulse 式键盘热力图数据源）。
static VK: [AtomicU64; 256] = {
    #[allow(clippy::declare_interior_mutable_const)]
    const Z: AtomicU64 = AtomicU64::new(0);
    [Z; 256]
};
/// 点击分键（0=left 1=right 2=middle 3=side1(XBUTTON1) 4=side2(XBUTTON2)）
static BUTTON: [AtomicU64; 5] = {
    #[allow(clippy::declare_interior_mutable_const)]
    const Z: AtomicU64 = AtomicU64::new(0);
    [Z; 5]
};
static SCROLL_TICKS: AtomicU64 = AtomicU64::new(0);
/// 注入滚轮刻度（审查 MEDIUM：滚轮连点器/自动化滚动不得伪造在场——
/// SCROLL_TICKS 的注入子集，presence 的 human 项扣减、auto 项计入）
static INJECTED_SCROLL_TICKS: AtomicU64 = AtomicU64::new(0);
/// 注入输入（自动化/合成）单独计数，绝不混入人的活动（三指标模型）
static INJECTED_KEYS: AtomicU64 = AtomicU64::new(0);
static INJECTED_CLICKS: AtomicU64 = AtomicU64::new(0);
static MOVES: AtomicU64 = AtomicU64::new(0);
static MOVE_DIST_PX: AtomicU64 = AtomicU64::new(0);
static SAMPLES: AtomicU64 = AtomicU64::new(0);
/// 键盘专属样本数（只随 key 事件增长；鼠标移动/滚轮不计入）。
/// keyboard 行的 samples 语义（审查 P2：此前输出全输入样本数，混入鼠标
/// 移动会把 agg 的 input_keys.count 抬高）。
static KEY_SAMPLES: AtomicU64 = AtomicU64::new(0);
// 上一采样点（计算移动距离用）：(x, y, valid) 打包进单个 AtomicU64，
// 布局 x:21bit<<43 | y:21bit<<22 | valid:1bit<<0（带符号 21bit，偏置 2^20；
// 坐标超出 ±2^20 时置 invalid——读回 None，与"上一点无效则跳过距离"一致，
// 且消除 (PREV_X, PREV_Y, PREV_VALID) 三个原子各自 swap 的撕裂窗口）。
static PREV_POS: AtomicU64 = AtomicU64::new(0);

const POS_BIAS: i64 = 1 << 20;
const POS_MASK: u64 = (1 << 21) - 1;

fn pack_pos(x: i32, y: i32) -> u64 {
    let xi = x as i64 + POS_BIAS;
    let yi = y as i64 + POS_BIAS;
    if (0..=(POS_MASK as i64)).contains(&xi) && (0..=(POS_MASK as i64)).contains(&yi) {
        ((xi as u64) << 43) | ((yi as u64) << 22) | 1
    } else {
        0 // 超范围：置无效
    }
}

fn unpack_pos(v: u64) -> Option<(i32, i32)> {
    if v & 1 == 0 {
        return None;
    }
    let x = ((v >> 43) & POS_MASK) as i64 - POS_BIAS;
    let y = ((v >> 22) & POS_MASK) as i64 - POS_BIAS;
    Some((x as i32, y as i32))
}

/// 键盘按下（press 计 1；release 不计，与 raw 形态的计数口径一致）。
pub fn record_key() {
    KEYS.fetch_add(1, Ordering::Relaxed);
    SAMPLES.fetch_add(1, Ordering::Relaxed);
    KEY_SAMPLES.fetch_add(1, Ordering::Relaxed);
    note_event_epoch_now();
}

/// 键盘按下并带 vk code（minute 模式热力图路径）。
/// injected = LLKHF_INJECTED（SendKeys/SendInput 等合成输入）——单独计数，
/// 供"人在场 vs 自动化活动"分离（三指标模型，审查用户需求）。
pub fn record_key_vk(vk: u32, injected: bool) {
    KEYS.fetch_add(1, Ordering::Relaxed);
    SAMPLES.fetch_add(1, Ordering::Relaxed);
    KEY_SAMPLES.fetch_add(1, Ordering::Relaxed);
    note_event_epoch_now();
    if injected {
        INJECTED_KEYS.fetch_add(1, Ordering::Relaxed);
    }
    // per-key 开关（默认开）：显式 opt-out 后不累计。keys 总数、samples、
    // injected 计数不受开关影响——APM/活跃分钟等计数语义完整保留。
    if vk_enabled() && (vk as usize) < 256 {
        VK[vk as usize].fetch_add(1, Ordering::Relaxed);
    }
}

/// 鼠标按下并分键（0=left 1=right 2=middle 3=side1 4=side2；release 不计）。
pub fn record_click_button(button: usize, injected: bool) {
    CLICKS.fetch_add(1, Ordering::Relaxed);
    SAMPLES.fetch_add(1, Ordering::Relaxed);
    note_event_epoch_now();
    if injected {
        INJECTED_CLICKS.fetch_add(1, Ordering::Relaxed);
    }
    if button < 5 {
        BUTTON[button].fetch_add(1, Ordering::Relaxed);
    }
}

/// 滚轮（ticks = |delta|/120 整数刻度数）。injected = LLMHF_INJECTED——
/// 注入滚轮单独计数，presence 不计入人在场（审查 MEDIUM：滚轮连点器）。
pub fn record_scroll(ticks: u64, injected: bool) {
    SCROLL_TICKS.fetch_add(ticks, Ordering::Relaxed);
    SAMPLES.fetch_add(1, Ordering::Relaxed);
    note_event_epoch_now();
    if injected {
        INJECTED_SCROLL_TICKS.fetch_add(ticks, Ordering::Relaxed);
    }
}

/// 鼠标移动（每事件记录：不做节流——原子计数无洪泛风险，且距离统计更准确）。
pub fn record_move(x: i32, y: i32) {
    // 单原子打包交换：读回的即"上一点"，无撕裂
    let prev = unpack_pos(PREV_POS.swap(pack_pos(x, y), Ordering::Relaxed));
    if let Some((px, py)) = prev {
        let d = (x - px).unsigned_abs() as u64 + (y - py).unsigned_abs() as u64;
        MOVE_DIST_PX.fetch_add(d, Ordering::Relaxed);
    }
    MOVES.fetch_add(1, Ordering::Relaxed);
    SAMPLES.fetch_add(1, Ordering::Relaxed);
    note_event_epoch_now();
}

// ─── 聚合线程侧（每秒一次 drain；关停时 flush_partial） ───────────────────────

#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct MinuteCounters {
    keys: u64,
    /// 键盘专属样本数（<= samples；keyboard 行 samples 的输出值）
    keys_samples: u64,
    clicks: u64,
    scroll_ticks: u64,
    moves: u64,
    move_dist_px: u64,
    samples: u64,
    /// per-key 频次（vk 索引；仅非零项参与 is_empty/add 语义，见下）
    vk: Vec<(u8, u64)>,
    /// 点击分键 [left, right, middle]
    buttons: [u64; 5],
    /// 注入输入（合成/自动化）子集计数，<= keys/clicks（三指标模型）
    injected_keys: u64,
    injected_clicks: u64,
    /// 注入滚轮刻度，<= scroll_ticks（同上）
    injected_scroll_ticks: u64,
}

impl MinuteCounters {
    fn is_empty(&self) -> bool {
        self.keys == 0
            && self.clicks == 0
            && self.scroll_ticks == 0
            && self.moves == 0
            && self.samples == 0
    }

    fn add(&mut self, o: &MinuteCounters) {
        self.keys += o.keys;
        self.keys_samples += o.keys_samples;
        self.clicks += o.clicks;
        self.injected_keys += o.injected_keys;
        self.injected_clicks += o.injected_clicks;
        self.injected_scroll_ticks += o.injected_scroll_ticks;
        self.scroll_ticks += o.scroll_ticks;
        self.moves += o.moves;
        self.move_dist_px += o.move_dist_px;
        self.samples += o.samples;
        for (k, v) in o.vk.iter() {
            if let Some(e) = self.vk.iter_mut().find(|(k2, _)| *k2 == *k) {
                e.1 += *v;
            } else {
                self.vk.push((*k, *v));
            }
        }
        for i in 0..5 {
            self.buttons[i] += o.buttons[i];
        }
    }
}

/// 当前分钟桶：本地时间截断到分钟 + 对应 UTC 起点时间戳。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MinuteKey {
    /// 本地分钟起点（second/纳秒清零）
    start_local: DateTime<Local>,
}

impl MinuteKey {
    fn of(now: DateTime<Local>) -> Self {
        Self {
            start_local: now
                .with_second(0)
                .and_then(|t| t.with_nanosecond(0))
                .unwrap_or(now),
        }
    }

    /// 事件时间分钟：纪元分钟 -> 本地分钟桶，超过 `cap`（当前分钟，防时钟
    /// 回拨/超前）时钳到 `cap`。纪元分钟 <= 0 视为无记录返回 None。
    fn from_epoch_min(mins: i64, cap: &MinuteKey) -> Option<Self> {
        if mins <= 0 {
            return None;
        }
        chrono::DateTime::from_timestamp(mins * 60, 0).map(|t| {
            let k = MinuteKey::of(t.with_timezone(&Local));
            if k.start_local > cap.start_local {
                *cap
            } else {
                k
            }
        })
    }

    fn timestamp_rfc3339(&self) -> String {
        self.start_local.with_timezone(&Utc).to_rfc3339()
    }
}

static PENDING: Mutex<Option<(MinuteKey, MinuteCounters)>> = Mutex::new(None);
/// 重启抑制（二轮审查重设计）：绑定**具体分钟**而非裸布尔。activate 时记下
/// 当前分钟，该分钟内不发事件（重启后计数器从零开始，秒级小快照会把库里
/// 已存的大终值覆盖回小值）；过期条件 = rollover 或 flush 遇到不同分钟，
/// 且被抑制分钟的计数**直接丢弃**（注释语义"≤59s 不计数"的严格实现）。
static SUPPRESS_MINUTE: Mutex<Option<MinuteKey>> = Mutex::new(None);
/// 秒级可见：drain 每秒被调用一次，把当前分钟累计值整体 UPSERT 覆盖到
/// 数据库同一行（0005 迁移的部分唯一索引）。派生缓存行的原地覆盖不违反
/// 原始数据只增铁律（铁律保护对象是 press/click 等原始事件）。
fn drain_atomics() -> MinuteCounters {
    let vk: Vec<(u8, u64)> = VK
        .iter()
        .enumerate()
        .map(|(i, c)| (i as u8, c.swap(0, Ordering::Relaxed)))
        .filter(|(_, v)| *v > 0)
        .collect();
    MinuteCounters {
        keys: KEYS.swap(0, Ordering::Relaxed),
        keys_samples: KEY_SAMPLES.swap(0, Ordering::Relaxed),
        clicks: CLICKS.swap(0, Ordering::Relaxed),
        scroll_ticks: SCROLL_TICKS.swap(0, Ordering::Relaxed),
        moves: MOVES.swap(0, Ordering::Relaxed),
        move_dist_px: MOVE_DIST_PX.swap(0, Ordering::Relaxed),
        samples: SAMPLES.swap(0, Ordering::Relaxed),
        injected_keys: INJECTED_KEYS.swap(0, Ordering::Relaxed),
        injected_clicks: INJECTED_CLICKS.swap(0, Ordering::Relaxed),
        injected_scroll_ticks: INJECTED_SCROLL_TICKS.swap(0, Ordering::Relaxed),
        vk,
        buttons: [
            BUTTON[0].swap(0, Ordering::Relaxed),
            BUTTON[1].swap(0, Ordering::Relaxed),
            BUTTON[2].swap(0, Ordering::Relaxed),
            BUTTON[3].swap(0, Ordering::Relaxed),
            BUTTON[4].swap(0, Ordering::Relaxed),
        ],
    }
}

/// 把 `counters` 折叠为该分钟的 input_agg 事件（键盘/鼠标各一行，空桶跳过）。
///
/// `final_row`（审查 P1：硬杀后的部分分钟快照残行修复）：true = 该分钟已完整
/// （由后续 drain 折叠的终值行）；false = 秒级快照 / flush_partial 的部分行。
/// 随行落库为 `$.final`，供重启时判定"抑制"还是"合并续写"（见
/// [`clear_restart_suppression`] 与 collector 启动查询）。
fn events_for(key: MinuteKey, c: &MinuteCounters, final_row: bool) -> Vec<Event> {
    let mut out = Vec::with_capacity(2);
    if c.is_empty() {
        return out;
    }
    if c.keys > 0 {
        // per-key 频次以紧凑 map 输出（"65": 12, ...），零内容零顺序零时间戳
        let vk_map: serde_json::Map<String, serde_json::Value> =
            c.vk.iter()
                .map(|(k, v)| (k.to_string(), serde_json::json!(v)))
                .collect();
        let mut kd = serde_json::json!({
            "keys": c.keys,
            // 键盘专属样本数（审查 P2：不再混入鼠标/滚轮样本）
            "samples": c.keys_samples,
            "keys_samples": c.keys_samples,
            "vk": vk_map,
            "final": final_row,
        });
        if c.injected_keys > 0 {
            kd["injected_keys"] = serde_json::json!(c.injected_keys);
        }
        out.push(
            Event::new(EventAction::InputAgg, EventType::Keyboard)
                .data(kd)
                .app("", ""),
        );
    }
    if c.clicks > 0 || c.scroll_ticks > 0 || c.moves > 0 || c.move_dist_px > 0 {
        let mut md = serde_json::json!({
            "clicks": c.clicks,
            "scroll_ticks": c.scroll_ticks,
            "moves": c.moves,
            "move_distance_px": c.move_dist_px,
            "samples": c.samples,
            "clicks_left": c.buttons[0],
            "clicks_right": c.buttons[1],
            "clicks_middle": c.buttons[2],
            "clicks_side1": c.buttons[3],
            "clicks_side2": c.buttons[4],
            "final": final_row,
        });
        if c.injected_clicks > 0 {
            md["injected_clicks"] = serde_json::json!(c.injected_clicks);
        }
        if c.injected_scroll_ticks > 0 {
            // 注入滚轮单列（presence human 项扣减，审查 MEDIUM）
            md["injected_scroll_ticks"] = serde_json::json!(c.injected_scroll_ticks);
        }
        out.push(
            Event::new(EventAction::InputAgg, EventType::Mouse)
                .data(md)
                .app("", ""),
        );
    }
    // 分钟起点对齐时间戳（查询端按分钟分桶时与活动分钟天然对齐）
    let ts = key.timestamp_rfc3339();
    for e in &mut out {
        e.timestamp = ts.clone();
    }
    out
}

/// 聚合线程每秒调用：把原子计数 drain 进当前分钟桶；
/// 若已跨入新分钟，把上一个完整分钟折叠成 input_agg 事件（`$.final: true`）。
///
/// 事件时间分桶（审查）：本批计数的落桶分钟取自 `LAST_EVENT_EPOCH_MIN`
/// （钳到当前分钟），聚合线程停滞跨分钟时计数仍归入事件发生的那一分钟。
pub fn drain(now_local: DateTime<Local>) -> Vec<Event> {
    // 先 drain 计数再取事件时间（全库审查 P1：旧顺序在两步之间到达的事件
    // 会被计入本批却带着上一分钟的 epoch，可能落进随即折叠的 final 行永久
    // 错桶；新顺序下这些边界事件归到 cur，最多错桶一分钟且可被后续覆盖）
    let drained = drain_atomics();
    let last_evt = LAST_EVENT_EPOCH_MIN.swap(0, Ordering::Relaxed);
    let cur = MinuteKey::of(now_local);
    let evt_key = MinuteKey::from_epoch_min(last_evt as i64, &cur).unwrap_or(cur);
    let mut out = Vec::new();
    // 锁中毒不应静默清零输入统计（审查 P2）：取回内部数据继续
    let mut g = PENDING.lock().unwrap_or_else(|e| e.into_inner());
    let mut sup = SUPPRESS_MINUTE.lock().unwrap_or_else(|e| e.into_inner());
    {
        // 抑制判定绑定具体分钟（二轮审查：裸布尔在 rollover/None 分支清不掉）
        let suppressing_cur = matches!(&*sup, Some(k) if *k == cur);
        match g.take() {
            Some((key, mut acc)) if key == cur => {
                acc.add(&drained);
                // 秒级可见：只要本分钟有输入，就把累计值整行 UPSERT（下游幂等覆盖）
                if !acc.is_empty() && !suppressing_cur {
                    out = events_for(key, &acc, false);
                }
                *g = Some((key, acc));
            }
            Some((key, acc)) => {
                // rollover：抑制判定必须针对**被折叠的分钟 key**（审查 P1：
                // 旧实现比较 sup == cur，rollover 时被折叠的是上一分钟，
                // 抑制永不命中 → 重启后被抑制分钟的计数照常写出）。
                let suppressing = matches!(&*sup, Some(k) if *k == key);
                let mut acc = acc;
                // 事件时间分桶：本批事件若仍属于被折叠的分钟（线程停滞跨
                // 分钟收集），并入后再折叠——计数落在事件时间分钟
                let merged_into_fold = evt_key == key;
                if merged_into_fold {
                    acc.add(&drained);
                }
                // 被抑制分钟攒的计数**直接丢弃**——它们是重启后
                // 的小值，写出去会把库里上一会话的大终值覆盖回小值
                // 审查 HIGH：时钟回拨防护——目标分钟已被本会话更晚的终值
                // 覆盖过（回拨落回已折叠分钟）时不得再回写（UPSERT 会把
                // 已发生的历史计数改掉），直接丢弃。
                if !suppressing && fold_allowed(&key) {
                    out = events_for(key, &acc, true);
                }
                // 无论是否被抑制/是否空桶，rollover 后该分钟在本会话内不再回写
                let m = key.start_local.with_timezone(&Utc).timestamp() / 60;
                LAST_FINALIZED_MIN.fetch_max(m as u64, Ordering::Relaxed);
                *sup = None;
                if evt_key == cur {
                    *g = Some((cur, drained));
                } else if merged_into_fold {
                    *g = Some((cur, MinuteCounters::default()));
                } else if fold_allowed(&evt_key) {
                    // 迟到事件属于更早的未建桶分钟：为该分钟开桶
                    *g = Some((evt_key, drained));
                } else {
                    // 迟到事件的目标分钟已被本会话更晚的终值覆盖（时钟回拨
                    // 防护，见 fold_allowed）：往该分钟开桶必被丢弃。计数是
                    // 本会话的新鲜输入，改记到当前分钟——历史行不被改写，
                    // 计数也不丢
                    *g = Some((cur, drained));
                }
            }
            None => {
                *g = Some((evt_key, drained));
            }
        }
    }
    out
}

/// 关停兜底：把当前**未满**分钟的部分计数立即折叠成事件（不留到下一分钟；
/// 行带 `$.final: false`，重启时可识别为部分快照）。
pub fn flush_partial(now_local: DateTime<Local>) -> Vec<Event> {
    PendingFlush::take(now_local).0
}

/// 可回滚的关停部分分钟 flush（审查 HIGH：flush_partial 是消费性的——
/// g.take() + swap(0)。聚合线程关停 flush 撞满通道被丢后，shutdown 的
/// "直写最后一份 partial"兜底读到的是已被消费掉的空状态，"最后一分钟不丢"
/// 恰在 writer 停滞+通道满的目标场景下失效。take() 与入队解耦：入队失败时
/// 调 restore() 把消费掉的计数与 pending 桶整体还回，shutdown 兜底仍拿得到）。
pub struct PendingFlush {
    counters: MinuteCounters,
    last_evt: u64,
    pending: Option<(MinuteKey, MinuteCounters)>,
}

impl PendingFlush {
    /// 消费当前状态并折叠为关停部分行（final=false）。
    pub fn take(now_local: DateTime<Local>) -> (Vec<Event>, PendingFlush) {
        // 与 drain 同序：先 drain 计数再取事件时间（见 drain 内注释）
        let counters = drain_atomics();
        let last_evt = LAST_EVENT_EPOCH_MIN.swap(0, Ordering::Relaxed);
        let cur = MinuteKey::of(now_local);
        // 统一锁获取顺序：先 PENDING 再 SUPPRESS（与 drain 一致，审查 P2：
        // 两函数顺序相反构成潜在死锁对）。
        let mut g = PENDING.lock().unwrap_or_else(|e| e.into_inner());
        let mut sup = SUPPRESS_MINUTE.lock().unwrap_or_else(|e| e.into_inner());
        // 快照 pending（restore 用）：无论下方走哪个分支，还原时整体放回
        let pending_snapshot = g.clone();
        // 抑制分钟内：残留计数直接丢弃（小值覆盖大终值的口子，二轮审查 P1）
        if matches!(sup.as_ref(), Some(k) if *k == cur) {
            *sup = None;
            return (
                Vec::new(),
                PendingFlush {
                    counters,
                    last_evt,
                    pending: pending_snapshot,
                },
            );
        }
        let (key, mut acc) = match g.take() {
            Some((k, a)) => (k, a),
            // 无 pending 桶时按**最后一次活动的分钟**（钳到当前）落桶，避免
            // 关停路径把最后几秒的计数归错分钟（事件时间分桶，审查）
            None => MinuteKey::from_epoch_min(last_evt as i64, &cur)
                .map(|k| (k, MinuteCounters::default()))
                .unwrap_or((cur, MinuteCounters::default())),
        };
        // 极端兜底：pending 桶与当前分钟不一致（聚合线程刚 rollover 但事件
        // 尚未落库），以 pending 桶为准输出，避免把计数归错分钟。
        acc.add(&counters);
        // 回归审查 P2：抑制判定也要覆盖 pending 桶分钟——聚合线程停滞跨分钟后
        // 关停时，pending key 等于被抑制的分钟（< cur），上面的 cur 判定拦不住，
        // 会把重启会话的小值当 final:false 写出去覆盖上一会话的大终值。
        let events = if matches!(sup.as_ref(), Some(k) if *k == key) {
            Vec::new()
        } else if !fold_allowed(&key) {
            // 审查 HIGH：时钟回拨防护——该分钟已被本会话更晚的终值落库，
            // 部分行回写同样会覆盖真实计数，丢弃
            Vec::new()
        } else {
            events_for(key, &acc, false)
        };
        (
            events,
            PendingFlush {
                counters,
                last_evt,
                pending: pending_snapshot,
            },
        )
    }

    /// 入队失败：把消费掉的原子计数、pending 桶与事件时间分钟原样还回。
    ///
    /// 关停时 hooks 已停（collector.shutdown 先 stop 再 join 聚合线程），
    /// 此处 store/fetch_add 无并发写者。若部分事件已入队、部分撞 Full，还原
    /// 后 shutdown 兜底会再写一次同键 UPSERT（同 (timestamp,event_type) 整行
    /// 覆盖、值相同），幂等收敛，不产生重复计数。
    pub fn restore(self) {
        use Ordering::Relaxed;
        KEYS.fetch_add(self.counters.keys, Relaxed);
        KEY_SAMPLES.fetch_add(self.counters.keys_samples, Relaxed);
        CLICKS.fetch_add(self.counters.clicks, Relaxed);
        SCROLL_TICKS.fetch_add(self.counters.scroll_ticks, Relaxed);
        MOVES.fetch_add(self.counters.moves, Relaxed);
        MOVE_DIST_PX.fetch_add(self.counters.move_dist_px, Relaxed);
        SAMPLES.fetch_add(self.counters.samples, Relaxed);
        INJECTED_KEYS.fetch_add(self.counters.injected_keys, Relaxed);
        INJECTED_CLICKS.fetch_add(self.counters.injected_clicks, Relaxed);
        INJECTED_SCROLL_TICKS.fetch_add(self.counters.injected_scroll_ticks, Relaxed);
        for (i, v) in self.counters.buttons.iter().enumerate() {
            BUTTON[i].fetch_add(*v, Relaxed);
        }
        for (vk, n) in &self.counters.vk {
            VK[*vk as usize].fetch_add(*n, Relaxed);
        }
        LAST_EVENT_EPOCH_MIN.store(self.last_evt, Relaxed);
        // pending 桶整体还原（take 消费前的快照）
        let mut g = PENDING.lock().unwrap_or_else(|e| e.into_inner());
        *g = self.pending;
    }
}

/// 重置全部聚合状态（采集器启动/重启时调用，避免跨会话串数）。
pub fn reset() {
    set_minute_mode(false);
    // per-key 开关一并复位到默认开启（本地数据完整优先）；采集器启动路径
    // reset 后会按 CollectorSettings.vk_frequency_enabled 重新设置
    set_vk_enabled(true);
    drain_atomics();
    for c in VK.iter() {
        c.store(0, Ordering::Relaxed);
    }
    for c in BUTTON.iter() {
        c.store(0, Ordering::Relaxed);
    }
    INJECTED_KEYS.store(0, Ordering::Relaxed);
    INJECTED_CLICKS.store(0, Ordering::Relaxed);
    INJECTED_SCROLL_TICKS.store(0, Ordering::Relaxed);
    KEY_SAMPLES.store(0, Ordering::Relaxed);
    PREV_POS.store(0, Ordering::Relaxed);
    LAST_EVENT_EPOCH_MIN.store(0, Ordering::Relaxed);
    // 会话内时钟回拨防护一并复位（reset = 新采集会话开始；跨会话的重启
    // 小快照覆盖问题由重启抑制 + $.final 判定负责，见模块文档）
    LAST_FINALIZED_MIN.store(0, Ordering::Relaxed);
    let mut g = PENDING.lock().unwrap_or_else(|e| e.into_inner());
    *g = None;
}

/// 进入 minute 聚合模式（仅 [`crate::collector::start_collection_with`] 调用）。
pub(crate) fn activate() {
    set_minute_mode(true);
    let mut sup = SUPPRESS_MINUTE.lock().unwrap_or_else(|e| e.into_inner());
    *sup = Some(MinuteKey::of(chrono::Local::now()));
}

/// 取消本会话的重启分钟抑制（合并续写模式，审查 P1：硬杀后部分分钟快照修复）。
///
/// 默认 [`activate`] 会抑制"当前分钟"——重启后秒级小快照会把库里已存的
/// 大终值覆盖回小值。但若库里该分钟只有**部分快照**（`$.final` 非 true，
/// 即上次会话被硬杀、终值从未写出），抑制会把那部分计数整体丢弃：此时应
/// 改为合并续写。是否可抑制由 collector 启动时查库判定（`$.final` 为 true
/// 或该分钟无行才保持抑制），判定通过后调用本函数解除。
pub fn clear_restart_suppression() {
    let mut sup = SUPPRESS_MINUTE.lock().unwrap_or_else(|e| e.into_inner());
    *sup = None;
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    /// 全局原子/Mutex 状态被全部测试共享，串行化避免互相清零。
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn guard() -> std::sync::MutexGuard<'static, ()> {
        TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn local_min(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Local> {
        Local
            .with_ymd_and_hms(y, mo, d, h, mi, 0)
            .earliest()
            .unwrap()
    }

    fn counter_of(evts: &[Event], etype: EventType, key: &str) -> u64 {
        evts.iter()
            .filter(|e| e.event_type == etype)
            .map(|e| {
                e.event_data
                    .as_ref()
                    .and_then(|v| v.get(key))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0)
            })
            .sum()
    }

    #[test]
    fn suppress_minute_survives_rollover() {
        let _g = guard();
        reset();
        activate(); // 抑制当前分钟（重启抑制路径）
        let t0 = MinuteKey::of(chrono::Local::now()).start_local;
        record_key();
        record_key();
        record_click_button(0, false);
        // 同分钟 drain：抑制中，不产出
        let evts = drain(t0);
        assert!(evts.is_empty(), "抑制分钟内的秒级 drain 不应产出事件");
        // 推进到下一分钟：被抑制分钟的累计必须整体丢弃（P1：判定针对被折叠 key）
        let t1 = MinuteKey::of(t0 + chrono::Duration::minutes(1)).start_local;
        let evts = drain(t1);
        assert!(evts.is_empty(), "被抑制分钟的事件不得在 rollover 时产出");
        // 抑制已解除：下一分钟正常计数并产出
        record_key();
        let t2 = MinuteKey::of(t1 + chrono::Duration::minutes(1)).start_local;
        assert!(drain(t2).is_empty(), "建桶不产出");
        let evts = drain(MinuteKey::of(t2 + chrono::Duration::minutes(1)).start_local);
        assert_eq!(counter_of(&evts, EventType::Keyboard, "keys"), 1);
    }

    #[test]
    fn keyboard_row_samples_are_keyboard_only() {
        let _g = guard();
        reset();
        record_key();
        record_key();
        record_move(0, 0); // 鼠标样本：不得计入 keyboard 行 samples
        record_scroll(1, false);
        let evts = drain(local_min(2026, 6, 15, 10, 30)); // 建桶
        assert!(evts.is_empty());
        let evts = flush_partial(local_min(2026, 6, 15, 10, 30));
        let kb = evts
            .iter()
            .find(|e| e.event_type == EventType::Keyboard)
            .expect("keyboard row");
        let d = kb.event_data.as_ref().unwrap();
        assert_eq!(d.get("keys_samples").and_then(|v| v.as_u64()), Some(2));
        assert_eq!(
            d.get("samples").and_then(|v| v.as_u64()),
            Some(2),
            "keyboard 行 samples = 键盘专属样本数"
        );
        let ms = evts
            .iter()
            .find(|e| e.event_type == EventType::Mouse)
            .expect("mouse row");
        assert_eq!(
            ms.event_data
                .as_ref()
                .unwrap()
                .get("samples")
                .and_then(|v| v.as_u64()),
            Some(4),
            "mouse 行 samples 仍为全输入样本数"
        );
    }

    #[test]
    fn prev_pos_packing_no_tear_and_range_guard() {
        // 打包/解包往返
        assert_eq!(unpack_pos(pack_pos(1920, 1080)), Some((1920, 1080)));
        assert_eq!(unpack_pos(pack_pos(-1920, -1080)), Some((-1920, -1080)));
        assert_eq!(unpack_pos(pack_pos(0, 0)), Some((0, 0)));
        // 超出 ±2^20：置无效
        assert_eq!(unpack_pos(pack_pos(i32::MAX, 0)), None);
        assert_eq!(unpack_pos(pack_pos(0, i32::MIN)), None);
        assert_eq!(unpack_pos(0), None);

        // record_move 语义：超范围点被置无效，使下一次距离计算跳过
        let _g = guard();
        reset();
        record_move(i32::MAX, 0); // 超范围：写 prev 时置无效
        record_move(3, 4); // 上一有效点已失效：距离不计
        record_move(3, 4); // 0
        record_move(6, 8); // 3+4=7
        let evts = drain(local_min(2026, 6, 15, 10, 30));
        assert!(evts.is_empty());
        let evts = flush_partial(local_min(2026, 6, 15, 10, 30));
        assert_eq!(counter_of(&evts, EventType::Mouse, "moves"), 4);
        assert_eq!(counter_of(&evts, EventType::Mouse, "move_distance_px"), 7);
    }

    #[test]
    fn counters_preserved_across_rollover() {
        let _g = guard();
        reset();
        let t0 = local_min(2026, 6, 15, 10, 30);
        // 第 1 分钟：事件分两次 drain 到达（2 键 1 点击 → 1 键 1 点击）
        record_key();
        record_key();
        record_click_button(0, false);
        let evts = drain(t0);
        assert!(evts.is_empty(), "首个桶未完成，不应产出事件");
        record_key();
        record_click_button(0, false);
        // 跨入下一分钟：第 1 分钟已 drain 进桶的计数（2 键 1 点击）折叠为 2 行
        let t1 = local_min(2026, 6, 15, 10, 31);
        let evts = drain(t1);
        assert_eq!(evts.len(), 2);
        assert_eq!(counter_of(&evts, EventType::Keyboard, "keys"), 2);
        assert_eq!(counter_of(&evts, EventType::Mouse, "clicks"), 1);
        // 时间戳对齐分钟起点
        assert!(evts
            .iter()
            .all(|e| e.timestamp == t0.with_timezone(&Utc).to_rfc3339()));
        // 第 2 分钟的计数（1 键 1 点击）在下次 rollover 时产出
        let evts = drain(local_min(2026, 6, 15, 10, 32));
        assert_eq!(counter_of(&evts, EventType::Keyboard, "keys"), 1);
        assert_eq!(counter_of(&evts, EventType::Mouse, "clicks"), 1);
    }

    #[test]
    fn mouse_counters_and_distance() {
        let _g = guard();
        reset();
        record_move(0, 0);
        record_move(3, 4); // 距离 5
        record_move(3, 4); // 距离 0（不动）
        record_scroll(2, false);
        let evts = drain(local_min(2026, 6, 15, 10, 30)); // 建桶
        assert!(evts.is_empty());
        let evts = flush_partial(local_min(2026, 6, 15, 10, 30)); // 同分钟 partial
        assert_eq!(evts.len(), 1);
        assert_eq!(evts[0].event_type, EventType::Mouse);
        assert_eq!(counter_of(&evts, EventType::Mouse, "moves"), 3);
        assert_eq!(
            counter_of(&evts, EventType::Mouse, "move_distance_px"),
            7,
            "曼哈顿距离：|3-0|+|4-0| + |0|+|0| = 7"
        );
        assert_eq!(counter_of(&evts, EventType::Mouse, "scroll_ticks"), 2);
        assert_eq!(counter_of(&evts, EventType::Mouse, "samples"), 4);
    }

    #[test]
    fn flush_partial_emits_incomplete_minute() {
        let _g = guard();
        reset();
        let t = local_min(2026, 6, 15, 22, 59);
        record_key();
        record_key();
        // 不等分钟走完直接 flush（关停路径）
        let evts = flush_partial(t);
        assert_eq!(evts.len(), 1);
        assert_eq!(counter_of(&evts, EventType::Keyboard, "keys"), 2);
        assert_eq!(counter_of(&evts, EventType::Keyboard, "samples"), 2);
        // flush 后状态清零：再 flush 不重复产出
        assert!(flush_partial(t).is_empty());
    }

    #[test]
    fn empty_minute_produces_no_rows() {
        let _g = guard();
        reset();
        let t = local_min(2026, 6, 15, 10, 30);
        drain(t); // 建空桶
        let evts = drain(local_min(2026, 6, 15, 10, 31));
        assert!(
            evts.is_empty(),
            "空分钟不应写行（活跃分钟语义依赖行的存在）"
        );
    }

    /// 审查 P1（硬杀部分快照修复）：`$.final` 区分"完整分钟终值"与
    /// "秒级快照/关停部分行"，供重启时判定抑制还是合并续写。
    #[test]
    fn final_flag_distinguishes_complete_minute_from_partial() {
        let _g = guard();
        reset();
        // 事件时间分桶语义下，事件落桶分钟由 LAST_EVENT_EPOCH_MIN 决定
        //（record_* 写真实时钟，测试需显式注入事件时间）。
        let t0 = local_min(2026, 6, 15, 10, 30);
        let t0_min = (t0.with_timezone(&Utc).timestamp() / 60) as u64;
        let t1 = local_min(2026, 6, 15, 10, 31);
        let t1_min = (t1.with_timezone(&Utc).timestamp() / 60) as u64;
        let t2 = local_min(2026, 6, 15, 10, 32);
        let t2_min = (t2.with_timezone(&Utc).timestamp() / 60) as u64;

        // 场景一：未满分钟关停 flush → $.final = false（部分快照）
        record_key();
        LAST_EVENT_EPOCH_MIN.store(t0_min, Ordering::Relaxed);
        let evts = flush_partial(t0);
        let kb = evts
            .iter()
            .find(|e| e.event_type == EventType::Keyboard)
            .expect("keyboard row (partial)");
        assert_eq!(
            kb.event_data
                .as_ref()
                .unwrap()
                .get("final")
                .and_then(|v| v.as_bool()),
            Some(false),
            "flush_partial 行必须是部分快照（final=false）"
        );
        assert_eq!(
            kb.event_data
                .as_ref()
                .unwrap()
                .get("keys")
                .and_then(|v| v.as_u64()),
            Some(1),
            "部分快照计数不得为 0（drain/事件时间换序写错时计数会漂移丢失）"
        );

        // 场景二：10:31 的事件随 10:32 的 drain 折叠 → 终值行 final=true
        record_key();
        LAST_EVENT_EPOCH_MIN.store(t1_min, Ordering::Relaxed);
        // 先跑一次 10:31 的 drain：建桶（部分可见行 final=false）
        let _ = drain(t1);
        // 10:32 的 drain 触发 rollover：10:31 折叠为终值
        record_key();
        LAST_EVENT_EPOCH_MIN.store(t2_min, Ordering::Relaxed);
        let evts = drain(t2);
        let kb = evts
            .iter()
            .find(|e| {
                e.event_type == EventType::Keyboard
                    && e.event_data
                        .as_ref()
                        .and_then(|d| d.get("final"))
                        .and_then(|v| v.as_bool())
                        == Some(true)
            })
            .expect("keyboard final row");
        assert_eq!(
            kb.event_data
                .as_ref()
                .unwrap()
                .get("final")
                .and_then(|v| v.as_bool()),
            Some(true),
            "rollover 折叠的完整分钟必须是终值行（final=true）"
        );
        assert_eq!(
            kb.event_data
                .as_ref()
                .unwrap()
                .get("keys")
                .and_then(|v| v.as_u64()),
            Some(1),
            "终值行计数必须等于该分钟的事件数"
        );
    }

    #[test]
    fn per_key_and_button_counts_roll_up() {
        let _g = guard();
        reset();
        // per-key 频次默认开启（reset 后即为 true），无需显式设置
        assert!(vk_enabled(), "per-key 频次默认必须开启");
        record_key_vk(65, false); // A
        record_key_vk(65, false);
        record_key_vk(66, false); // B
        record_key_vk(9999, false); // 越界安全忽略
        record_click_button(0, false);
        record_click_button(2, false);
        let evts = drain(local_min(2026, 6, 15, 10, 30)); // 建桶
        assert!(evts.is_empty());
        let evts = flush_partial(local_min(2026, 6, 15, 10, 30));
        let kb = evts
            .iter()
            .find(|e| e.event_type == EventType::Keyboard)
            .expect("keyboard row");
        let vk = kb
            .event_data
            .as_ref()
            .and_then(|v| v.get("vk"))
            .expect("vk map");
        assert_eq!(vk.get("65").and_then(|v| v.as_u64()), Some(2));
        assert_eq!(vk.get("66").and_then(|v| v.as_u64()), Some(1));
        assert!(vk.get("9999").is_none(), "越界 vk 不入库");
        let total: u64 = vk
            .as_object()
            .unwrap()
            .values()
            .filter_map(|v| v.as_u64())
            .sum();
        assert_eq!(total, 3);
        let ms = evts
            .iter()
            .find(|e| e.event_type == EventType::Mouse)
            .expect("mouse row");
        let d = ms.event_data.as_ref().expect("mouse data");
        assert_eq!(d.get("clicks_left").and_then(|v| v.as_u64()), Some(1));
        assert_eq!(d.get("clicks_middle").and_then(|v| v.as_u64()), Some(1));
        assert_eq!(d.get("clicks_right").and_then(|v| v.as_u64()), Some(0));
    }

    /// opt-out 路径：显式关闭后 keys 总数照常累计（计数语义不变），
    /// 但 vk map 输出为空。默认开启路径见 per_key_and_button_counts_roll_up。
    #[test]
    fn vk_frequency_empty_when_explicitly_disabled() {
        let _g = guard();
        reset(); // reset 回到默认开启
        set_vk_enabled(false); // 显式关闭（opt-out）
        assert!(!vk_enabled(), "显式关闭后必须停止 per-key 记录");
        record_key_vk(65, false);
        record_key_vk(66, false);
        let evts = drain(local_min(2026, 6, 15, 10, 30)); // 建桶
        assert!(evts.is_empty());
        let evts = flush_partial(local_min(2026, 6, 15, 10, 30));
        let kb = evts
            .iter()
            .find(|e| e.event_type == EventType::Keyboard)
            .expect("keyboard row");
        let d = kb.event_data.as_ref().unwrap();
        // 计数语义完整：keys 总数不受开关影响
        assert_eq!(d.get("keys").and_then(|v| v.as_u64()), Some(2));
        // vk map 为空（无 per-key 记录）
        let vk = d.get("vk").expect("vk 键仍存在（空 map）");
        assert!(vk.as_object().unwrap().is_empty(), "关闭时 vk map 必须为空");
    }

    #[test]
    fn mode_switch_and_reset() {
        let _g = guard();
        reset();
        assert!(!minute_mode(), "默认必须为 raw 模式");
        activate();
        assert!(minute_mode());
        record_key();
        reset();
        assert!(!minute_mode());
        // reset 清计数
        let evts = flush_partial(local_min(2026, 6, 15, 10, 30));
        assert!(evts.is_empty());
    }
}
