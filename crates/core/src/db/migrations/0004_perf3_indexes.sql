-- 0004: perf3 长跑审计索引（2026-09-09 perf3-zipf 真实分布 1M 事件库实测驱动）
--
-- 症状（1200 应用 Zipf 分布、1M 事件、agg 缓存完备）：
-- - get_anomalies(7d) p95 537ms（目标 50ms）：new_app_surge 的 app_history_totals
--   按 bucket_id 过滤 agg_daily，而 agg_daily 主键是 (date, bucket_id)——bucket_id
--   不在前缀位，每次调用全表扫描 agg_daily（30 天 × 1200 应用 = 36k 行），
--   top20 应用 × 7 天 = 140 次全扫 ≈ 5M 行访问。
-- - get_summary[apps] p95 320ms：COUNT(DISTINCT app_name) 走 idx_events_timestamp
--   范围扫描后需回表取 app_name，1 天 ~33k 行的 rowid 回表在行散布于 375MB 库时
--   退化为随机页访问。
--
-- 修复（纯增量索引，无行为变化）：
CREATE INDEX IF NOT EXISTS idx_agg_daily_bucket_date ON agg_daily(bucket_id, date);
CREATE INDEX IF NOT EXISTS idx_events_ts_app ON events(timestamp, app_name);
