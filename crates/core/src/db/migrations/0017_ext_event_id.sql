-- 0017: events 加 ext_event_id 列（PRD v1.0 F04 编辑器扩展，幂等摄取键）
--
-- vscode-extension / 后续 Cursor 扩展经 dash POST /api/ext/events 上报编辑器
-- 事件。扩展侧有重试/断线重发，端点必须幂等：ext_event_id = "{source_id}:{event_id}"
-- （source_id ∈ {'vscode_ext','cursor_ext'}，event_id 由扩展侧 UUID 生成）。
-- 部分唯一索引（仅非 NULL 行）保证同键二次提交被忽略（INSERT OR IGNORE），
-- 不产生重复行、不触碰既有数据——append-only 铁律不受影响（只加列、只加索引）。
--
-- 事件本体仍写进现有 events 表（铁律：不建第二真相表）：event_type='editor'，
-- 派生进 activity_intervals 的 editor_session 区间（rebuild 从本列派生，
-- quality=observed；尾部未闭合按重建时刻收口为 unknown）。
ALTER TABLE events ADD COLUMN ext_event_id TEXT;

CREATE UNIQUE INDEX IF NOT EXISTS idx_events_ext_event_id
    ON events(ext_event_id) WHERE ext_event_id IS NOT NULL;
