-- 0008：action 定向查询索引
--
-- 按 event_action（如 device_snapshot / process_snapshot / input_agg）取最新
-- 值或做时间窗过滤的查询（current_state / snapshot 读路径）此前只能走
-- idx_events_type_action_ts（event_type 前导），action 选择性被 type 掺杂稀释。
-- (event_action, id DESC) 让"该 action 最新 N 行"退化为索引顺序扫描。
CREATE INDEX IF NOT EXISTS idx_events_action_id ON events(event_action, id DESC);
