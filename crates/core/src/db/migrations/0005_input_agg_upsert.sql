-- 0005: 键鼠分钟聚合行的秒级 UPSERT 支持（2026-09-10；2026-09-11 修订）
--
-- 设计变更：input_agg 是**派生聚合缓存**（不是原始事件），其"当前分钟"行
-- 现在每秒被累计值 UPSERT 覆盖一次，使仪表盘键鼠数据 ~1-2 秒可见。
-- 原始事件（press/click/switch/...）依然严格只增不改——铁律保护对象不变。
--
-- 修订记录：初版去重 DELETE 缺外层 event_action 过滤，NOT IN 会把所有
-- 非 input_agg 行全部删掉（审查抓到的 P0）。现版：先建索引（让 DELETE
-- 走部分索引，避免大库全表扫），清洗严格限定 input_agg 行，且保留
-- MAX(rowid)（同组最新累计终值，MIN 会保最早最小的快照）。
CREATE UNIQUE INDEX IF NOT EXISTS ux_input_agg_minute
  ON events(timestamp, event_type) WHERE event_action = 'input_agg';
DELETE FROM events WHERE event_action = 'input_agg' AND rowid NOT IN (
  SELECT MAX(rowid) FROM events
  WHERE event_action = 'input_agg'
  GROUP BY timestamp, event_type
);
