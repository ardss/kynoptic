//! 输入事件分钟聚合（opt-in `input_granularity: "minute"`）
//!
//! **默认是 raw**：Hook 回调与旧行为完全一致（逐事件入队落库）。仅在
//! [`crate::collector::CollectorSettings::input_granularity`] 设为 [`InputGranularity::Minute`]
//! 时，Hook 回调退化为纯原子计数（本模块 `record_*`），由采集器的聚合线程每秒
//! drain 一次、按**本地分钟桶**折叠成每桶一行的 `input_agg` 计数型事件：
//! - keyboard 行：`{"keys": K, "samples": S}`
//! - mouse 行：`{"clicks": C, "scroll_ticks": T, "moves": M, "move_distance_px": D, "samples": S}`
//!
//! 下游计数查询（queries::*）对两种形态统一兼容（见 queries::KEYS_ROW_EXPR）。
//! 计数语义保持：APM、活跃分钟、daily_agg 等只依赖计数，不受粒度影响；
//! 按键明细/鼠标坐标热力图在 minute 粒度下无原始样本，属已知取舍。
//!
//! 原始数据神圣性：minute 模式只是**生成侧**的折叠，不删除、不改写任何已落库
//! 数据；切回 raw 即恢复逐事件存储。

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::sync::Mutex;

use chrono::{DateTime, Local, Timelike, Utc};

use crate::types::{Event, EventAction, EventType};

// ─── Hook 回调侧（每事件只有原子操作，无锁无分配） ─────────────────────────────

static MINUTE_MODE: AtomicBool = AtomicBool::new(false);

/// 当前是否处于 minute 聚合模式（Hook 回调每次调用读取，须保持廉价）。
pub(crate) fn minute_mode() -> bool {
    MINUTE_MODE.load(Ordering::Relaxed)
}

fn set_minute_mode(on: bool) {
    MINUTE_MODE.store(on, Ordering::Relaxed);
}

static KEYS: AtomicU64 = AtomicU64::new(0);
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
static MOVES: AtomicU64 = AtomicU64::new(0);
static MOVE_DIST_PX: AtomicU64 = AtomicU64::new(0);
static SAMPLES: AtomicU64 = AtomicU64::new(0);
// 上一采样点（计算移动距离用）
static PREV_X: AtomicI32 = AtomicI32::new(0);
static PREV_Y: AtomicI32 = AtomicI32::new(0);
static PREV_VALID: AtomicBool = AtomicBool::new(false);

/// 键盘按下（press 计 1；release 不计，与 raw 形态的计数口径一致）。
pub fn record_key() {
    KEYS.fetch_add(1, Ordering::Relaxed);
    SAMPLES.fetch_add(1, Ordering::Relaxed);
}

/// 键盘按下并带 vk code（minute 模式热力图路径）。
pub fn record_key_vk(vk: u32) {
    KEYS.fetch_add(1, Ordering::Relaxed);
    SAMPLES.fetch_add(1, Ordering::Relaxed);
    if (vk as usize) < 256 {
        VK[vk as usize].fetch_add(1, Ordering::Relaxed);
    }
}

/// 鼠标按下并分键（0=left 1=right 2=middle 3=side1 4=side2；release 不计）。
pub fn record_click_button(button: usize) {
    CLICKS.fetch_add(1, Ordering::Relaxed);
    SAMPLES.fetch_add(1, Ordering::Relaxed);
    if button < 5 {
        BUTTON[button].fetch_add(1, Ordering::Relaxed);
    }
}

/// 滚轮（ticks = |delta|/120 整数刻度数）。
pub fn record_scroll(ticks: u64) {
    SCROLL_TICKS.fetch_add(ticks, Ordering::Relaxed);
    SAMPLES.fetch_add(1, Ordering::Relaxed);
}

/// 鼠标移动（每事件记录：不做节流——原子计数无洪泛风险，且距离统计更准确）。
pub fn record_move(x: i32, y: i32) {
    let px = PREV_X.swap(x, Ordering::Relaxed);
    let py = PREV_Y.swap(y, Ordering::Relaxed);
    if PREV_VALID.load(Ordering::Relaxed) {
        let d = (x - px).unsigned_abs() as u64 + (y - py).unsigned_abs() as u64;
        MOVE_DIST_PX.fetch_add(d, Ordering::Relaxed);
    }
    PREV_VALID.store(true, Ordering::Relaxed);
    MOVES.fetch_add(1, Ordering::Relaxed);
    SAMPLES.fetch_add(1, Ordering::Relaxed);
}

// ─── 聚合线程侧（每秒一次 drain；关停时 flush_partial） ───────────────────────

#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct MinuteCounters {
    keys: u64,
    clicks: u64,
    scroll_ticks: u64,
    moves: u64,
    move_dist_px: u64,
    samples: u64,
    /// per-key 频次（vk 索引；仅非零项参与 is_empty/add 语义，见下）
    vk: Vec<(u8, u64)>,
    /// 点击分键 [left, right, middle]
    buttons: [u64; 5],
}

impl MinuteCounters {
    fn is_empty(&self) -> bool {
        self.keys == 0
            && self.clicks == 0
            && self.scroll_ticks == 0
            && self.moves == 0
            && self.samples == 0
    }

    fn add(&mut self, o: MinuteCounters) {
        self.keys += o.keys;
        self.clicks += o.clicks;
        self.scroll_ticks += o.scroll_ticks;
        self.moves += o.moves;
        self.move_dist_px += o.move_dist_px;
        self.samples += o.samples;
        for (k, v) in o.vk {
            if let Some(e) = self.vk.iter_mut().find(|(k2, _)| *k2 == k) {
                e.1 += v;
            } else {
                self.vk.push((k, v));
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

    fn timestamp_rfc3339(&self) -> String {
        self.start_local.with_timezone(&Utc).to_rfc3339()
    }
}

static PENDING: Mutex<Option<(MinuteKey, MinuteCounters)>> = Mutex::new(None);
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
        clicks: CLICKS.swap(0, Ordering::Relaxed),
        scroll_ticks: SCROLL_TICKS.swap(0, Ordering::Relaxed),
        moves: MOVES.swap(0, Ordering::Relaxed),
        move_dist_px: MOVE_DIST_PX.swap(0, Ordering::Relaxed),
        samples: SAMPLES.swap(0, Ordering::Relaxed),
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
fn events_for(key: MinuteKey, c: &MinuteCounters) -> Vec<Event> {
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
        out.push(
            Event::new(EventAction::InputAgg, EventType::Keyboard)
                .data(serde_json::json!({
                    "keys": c.keys,
                    "samples": c.samples,
                    "vk": vk_map,
                }))
                .app("", ""),
        );
    }
    if c.clicks > 0 || c.scroll_ticks > 0 || c.moves > 0 || c.move_dist_px > 0 {
        out.push(
            Event::new(EventAction::InputAgg, EventType::Mouse)
                .data(serde_json::json!({
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
                }))
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
/// 若已跨入新分钟，把上一个完整分钟折叠成 input_agg 事件返回。
pub fn drain(now_local: DateTime<Local>) -> Vec<Event> {
    let drained = drain_atomics();
    let cur = MinuteKey::of(now_local);
    let mut out = Vec::new();
    // 锁中毒不应静默清零输入统计（审查 P2）：取回内部数据继续
    let mut g = PENDING.lock().unwrap_or_else(|e| e.into_inner());
    {
        match g.take() {
            Some((key, mut acc)) if key == cur => {
                acc.add(drained);
                // 秒级可见：只要本分钟有输入，就把累计值整行 UPSERT（下游幂等覆盖）
                if !acc.is_empty() {
                    out = events_for(key, &acc);
                }
                *g = Some((key, acc));
            }
            Some((key, acc)) => {
                out = events_for(key, &acc);
                *g = Some((cur, drained));
            }
            None => {
                *g = Some((cur, drained));
            }
        }
    }
    out
}

/// 关停兜底：把当前**未满**分钟的部分计数立即折叠成事件（不留到下一分钟）。
pub fn flush_partial(now_local: DateTime<Local>) -> Vec<Event> {
    let drained = drain_atomics();
    let cur = MinuteKey::of(now_local);
    let mut g = PENDING.lock().unwrap_or_else(|e| e.into_inner());
    let (key, mut acc) = match g.take() {
        Some((k, a)) => (k, a),
        None => (cur, MinuteCounters::default()),
    };
    // 极端兜底：pending 桶与当前分钟不一致（聚合线程刚 rollover 但事件
    // 尚未落库），以 pending 桶为准输出，避免把计数归错分钟。
    acc.add(drained);
    events_for(key, &acc)
}

/// 重置全部聚合状态（采集器启动/重启时调用，避免跨会话串数）。
pub fn reset() {
    set_minute_mode(false);
    drain_atomics();
    for c in VK.iter() {
        c.store(0, Ordering::Relaxed);
    }
    for c in BUTTON.iter() {
        c.store(0, Ordering::Relaxed);
    }
    PREV_VALID.store(false, Ordering::Relaxed);
    PREV_X.store(0, Ordering::Relaxed);
    PREV_Y.store(0, Ordering::Relaxed);
    let mut g = PENDING.lock().unwrap_or_else(|e| e.into_inner());
    *g = None;
}

/// 进入 minute 聚合模式（仅 [`crate::collector::start_collection_with`] 调用）。
pub(crate) fn activate() {
    set_minute_mode(true);
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
    fn counters_preserved_across_rollover() {
        let _g = guard();
        reset();
        let t0 = local_min(2026, 6, 15, 10, 30);
        // 第 1 分钟：事件分两次 drain 到达（2 键 1 点击 → 1 键 1 点击）
        record_key();
        record_key();
        record_click_button(0);
        let evts = drain(t0);
        assert!(evts.is_empty(), "首个桶未完成，不应产出事件");
        record_key();
        record_click_button(0);
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
        record_scroll(2);
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

    #[test]
    fn per_key_and_button_counts_roll_up() {
        let _g = guard();
        reset();
        record_key_vk(65); // A
        record_key_vk(65);
        record_key_vk(66); // B
        record_key_vk(9999); // 越界安全忽略
        record_click_button(0);
        record_click_button(2);
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
