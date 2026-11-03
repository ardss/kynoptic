-- 0014: 统一事件结构 + activity_intervals 派生表（PRD v1.0 跨生态活动感知，Stage 1）
--
-- events 扩充方式 = 加列而非新表（stage0-audit §5 决策 1）：
--   source_id  事件来源生态（'windows' / 'browser' / 'ai' …）；存量行回填 'windows'
--   device_id  机器级 UUID（存 metadata 表首次生成，本迁移只加列不赋值）
--   quality    质量标记列。存量行回填 'observed'（旧采集无质量概念，即
--              "观察到什么记什么"）。
--
-- quality 词表收口（stage0-audit §5 决策 5 的落点说明，双词表并存是有意
-- 分层、不是漂移）：
--   * events.quality / activity_intervals.quality 存储层词表 =
--     observed / estimated / unknown / confirmed
--     （observed=原始证据闭环；estimated=与 away 相交只打标；unknown=断线
--     收口缺口；confirmed 为 M1 词表兼容保留值）
--   * M1 四级词表（confirmed/estimated/incomplete/unavailable）只存在于
--     对外 evidence_level 映射层（crates/dash/src/lib.rs api_unified_timeline：
--     observed|confirmed→observed，estimated|inferred→inferred，其余→unknown），
--     不落库。存储层不以 M1 词表写入，避免「词表名撞库值」的假一致。
-- 只加列、只回填，不删不重排任何既有行（append-only 铁律），也不建第二真相表。
ALTER TABLE events ADD COLUMN source_id TEXT;
ALTER TABLE events ADD COLUMN device_id TEXT;
ALTER TABLE events ADD COLUMN quality TEXT;

-- 存量回填（幂等：只补 NULL，不覆盖显式值）
UPDATE events SET source_id = 'windows' WHERE source_id IS NULL;
UPDATE events SET quality = 'observed' WHERE quality IS NULL;

-- activity_intervals：统一时间轴的派生区间表（stage0-audit §3：**只读派生缓存**，
-- 不进真相链，可由 events 全量重建——rebuild_activity_intervals_at）。
--
-- interval_kind：
--   foreground_dwell     前台驻留（window/switch 去重推导；重建口径见 Rust 侧）
--   tab_foreground_dwell 浏览器 tab 前台驻留（Stage 2 F02 扩展接入后启用，暂不产出）
--   away                 离场（idle_start/idle_end 对推导）
-- quality：observed = 有事件证据闭环；estimated = 与 away 区间相交（在场存疑）；
--   unknown = 断线收口（末段无后续 switch 佐证，按重建时刻 now 收口）。
CREATE TABLE IF NOT EXISTS activity_intervals (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    interval_kind TEXT NOT NULL,
    start_ts      TEXT NOT NULL,
    end_ts        TEXT NOT NULL,
    app_name      TEXT,
    title_hash    TEXT,
    quality       TEXT NOT NULL DEFAULT 'observed',
    source_id     TEXT
);

CREATE INDEX IF NOT EXISTS idx_activity_intervals_kind_start
    ON activity_intervals(interval_kind, start_ts);
