-- 0009：input_agg 时间窗复合索引
--
-- minute_classification（overview/timeline 共用）按
-- (event_action='input_agg', timestamp 范围) 过滤。现有 idx_events_action_id
-- 的第二列是 id，对时间范围无过滤能力，查询会扫全部历史 input_agg 行——
-- 数据量一年后外推 200-500ms/次。(event_action, timestamp) 让时间窗退化为
-- 索引范围扫描，只触碰窗口内的行。
CREATE INDEX IF NOT EXISTS idx_events_action_ts ON events(event_action, timestamp);
