-- 0011: actor 证据缓存（0.4.0 F1，GPT 契约「缓存边界」勘误版）
--
-- 只存**可由单条输入事件推导**的分钟级证据计数（E_h/E_a 的原料）：
--   keys_total/keys_injected   键盘输入（raw press 按行累加；input_agg 快照 MAX 合并）
--   clicks_total/clicks_injected 鼠标点击（同上双语义）
--   scroll_total/scroll_injected 滚轮刻度（仅 input_agg mouse 行快照；raw 滚轮行
--     不入缓存——与 presence 权威口径一致，raw 滚轮不参与分钟分类）
--   max_event_rowid            已累计到的最大 events.id（幂等防护，与 agg_minute 同构）
--
-- actor / channel / 前台覆盖（E_f）等最终标签**不入本表**——它们依赖窗口
-- switch 的成对区间推导，不是单条输入事件可独立推导的，按 GPT 契约在查询层
-- 结合前台区间计算（见 queries/actor.rs）。本表是只读派生缓存，可随时由
-- events 全量重建；重建/回填路径与增量路径逐行同语义（复用 queries::*_ROW_EXPR）。
--
-- 不改写 agg_minute / agg_daily 任何行；旧指标语义零变化（0.4.0 契约）。

CREATE TABLE IF NOT EXISTS agg_minute_actor (
    date            TEXT NOT NULL,
    hour            INTEGER NOT NULL,
    minute          INTEGER NOT NULL,
    keys_total      INTEGER NOT NULL DEFAULT 0,
    keys_injected   INTEGER NOT NULL DEFAULT 0,
    clicks_total    INTEGER NOT NULL DEFAULT 0,
    clicks_injected INTEGER NOT NULL DEFAULT 0,
    scroll_total    INTEGER NOT NULL DEFAULT 0,
    scroll_injected INTEGER NOT NULL DEFAULT 0,
    max_event_rowid INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (date, hour, minute)
);

CREATE INDEX IF NOT EXISTS idx_agg_minute_actor_date ON agg_minute_actor(date);
