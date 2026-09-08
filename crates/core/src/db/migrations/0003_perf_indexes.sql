-- 0003: 查询性能覆盖索引（perf-query 1M 事件库 2026-09 基准驱动）
--
-- 症状：get_anomalies(7d) p95 ~4.8s（目标 50ms）。
-- 根因：按 (event_type, timestamp 区间) / (app_name) 过滤的查询只能用
-- idx_events_type_action_ts 的第一列或回表扫描，1M 行时退化为大范围扫描。
--
-- 修复（纯增量索引，无行为变化）：
-- - idx_events_type_ts (event_type, timestamp)：覆盖 event_type + 时间范围
--   查询（异常检测的按分钟聚合 / 活跃分钟 DISTINCT），实测 88ms → 4.6ms。
-- - idx_events_app_ts (app_name, timestamp)：覆盖 per-app 历史统计
--   （new_app_surge 的 app_history_totals），实测 208ms → 48ms。
CREATE INDEX IF NOT EXISTS idx_events_type_ts ON events(event_type, timestamp);
CREATE INDEX IF NOT EXISTS idx_events_app_ts ON events(app_name, timestamp);
