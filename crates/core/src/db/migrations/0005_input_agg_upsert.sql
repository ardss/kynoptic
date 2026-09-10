-- 0005: 键鼠分钟聚合行的秒级 UPSERT 支持（2026-09-10）
--
-- 设计变更：input_agg 是**派生聚合缓存**（不是原始事件），其"当前分钟"行
-- 现在每秒被累计值 UPSERT 覆盖一次，使仪表盘键鼠数据 ~1-2 秒可见。
-- 原始事件（press/click/switch/...）依然严格只增不改——铁律保护对象不变。
--
-- 纯增量：唯一部分索引 + 清洗。清洗范围【严格限定 input_agg 行】：
-- 外层必须带 event_action 过滤——否则 NOT IN 会把所有非 input_agg 行
-- 全部删掉（审查抓到的 P0，历史版本正中此雷）。input_agg 本身是
-- 派生缓存，重复行去重不触碰原始数据。
DELETE FROM events WHERE event_action = 'input_agg' AND rowid NOT IN (
  SELECT MIN(rowid) FROM events
  WHERE event_action = 'input_agg'
  GROUP BY timestamp, event_type
);
CREATE UNIQUE INDEX IF NOT EXISTS ux_input_agg_minute
  ON events(timestamp, event_type) WHERE event_action = 'input_agg';
