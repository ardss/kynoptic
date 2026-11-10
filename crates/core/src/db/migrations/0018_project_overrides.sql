-- 0018: project_overrides 表 + activity_intervals.project_source 列
-- （PRD v1.0 F06 缺口补齐：人工修正与可追溯）

-- project_overrides：人工项目指定记录（append-only 派生层）。
--
-- 与 activity_intervals 同属派生层（不进真相链，stage0-audit §3）：events
-- 仍是唯一真相源；本表是用户对区间→项目归属的**人工修正指令**，每次指定
-- 追加一行、从不 UPDATE/DELETE（修正再指定 = 新行，corrected_from 记录被
-- 取代的前归属）。派生表可随时由 events + 本表全量重建，因此"可重建"。
--
-- 匹配语义（rebuild 时消费）：区间 (interval_kind, app_name, title_hash)
-- 相同且区间 [start_ts, end_ts] 与本行窗口相交即命中；多条命中取 id 最大
-- （最晚一次人工指定，append-only 最新覆盖）。
--
-- 字段：
-- - corrected_from：指定前的归属来源（'rule' / 'manual' / 'unassigned'），
--   审计用，由服务端在写入时如实记录；
-- - source：固定 'manual'（与规则推断 'rule' 区分）；
-- - created_at：写入时刻（UTC RFC3339，注入时钟）；
-- - created_by：操作面（'dashboard' / 'cli'…，防呆默认 dashboard）；
-- - note：可选备注（截断在服务端）。
CREATE TABLE IF NOT EXISTS project_overrides (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    interval_kind TEXT NOT NULL,
    start_ts TEXT NOT NULL,
    end_ts TEXT NOT NULL,
    app_name TEXT NOT NULL,
    title_hash TEXT,
    project TEXT NOT NULL,
    corrected_from TEXT NOT NULL DEFAULT 'unassigned',
    source TEXT NOT NULL DEFAULT 'manual',
    created_at TEXT NOT NULL,
    created_by TEXT NOT NULL DEFAULT 'dashboard',
    note TEXT
);
CREATE INDEX IF NOT EXISTS idx_project_overrides_kind_app
    ON project_overrides (interval_kind, app_name);

-- activity_intervals 加 project_source 列：项目归属来源
-- 'manual'（人工指定，证据 observed）/ 'rule'（规则命中，证据 inferred）/
-- NULL（未命中任何归属，无证据不猜——查询侧按 unassigned 展示）。
-- 派生表可全量重建：加列 + 重建即生效，无数据回填；append-only 铁律不受
-- 影响（本表是派生缓存，只加列不删行，先例 0015/0016）。
ALTER TABLE activity_intervals ADD COLUMN project_source TEXT;
