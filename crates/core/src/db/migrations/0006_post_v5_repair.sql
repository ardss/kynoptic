-- 0006: 存量修复迁移（2026-09-11）
--
-- 背景：0005 的第一版把去重 DELETE 写成了 `NOT IN (只含 input_agg 的子查询)`
-- 而没有外层 event_action 过滤，任何在 v0.1.0 早期二进制上做过"带历史数据
-- 升级"的库已被误删原始事件（P0，不可恢复）。本迁移给所有 schema_version=5
-- 的存量库兜底：
--   1) 确保 ux_input_agg_minute 存在（老二进制可能因中途失败没建上）；
--   2) 以安全版去重（严格限定 input_agg 行、保 MAX(rowid)=最新累计终值），
--      重复行本不该存在，存在即损伤，清掉防止 UPSERT 撞索引静默失败。
-- 原始事件（press/click/switch）本迁移绝不触碰。
CREATE UNIQUE INDEX IF NOT EXISTS ux_input_agg_minute
  ON events(timestamp, event_type) WHERE event_action = 'input_agg';
DELETE FROM events WHERE event_action = 'input_agg' AND rowid NOT IN (
  SELECT MAX(rowid) FROM events
  WHERE event_action = 'input_agg'
  GROUP BY timestamp, event_type
);
