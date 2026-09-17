-- 0010：device_snapshot「最新含字段快照」部分索引
--
-- api_input（$.input_devices）与 overview 硬件卡（$.memory.total_gb）的
-- "最新含字段快照"查询按 (event_action='device_snapshot' AND
-- json_extract(...) IS NOT NULL) 过滤后 ORDER BY id DESC LIMIT 1。
-- 0008 的 idx_events_action_id 第二列是 id，但 device_snapshot 里不含目标
-- 字段的行同样在索引里——无匹配行时（全新机器 / 拓扑从未变化）反向扫全部
-- 历史 device_snapshot 行（90 天库实测 ~1.7s，随历史线性恶化）。
--
-- 部分索引把"含字段"的行单独建成 (event_action, id DESC) 序，无匹配行时
-- 索引为空、查询退化为 O(1) 空扫。
--
-- 注意：部分索引的 WHERE 谓词必须与查询 WHERE **语义完全同形**才能命中
-- （SQLite 按表达式等价匹配）——dash 侧 overview 硬件卡查询已同步去掉冗余的
-- json_valid(event_data) 条件以保持同形（json_extract 对 NULL/合法 JSON 之外
-- 不产生该谓词误判；event_data 列只写 serde_json 序列化结果或 NULL）。
CREATE INDEX IF NOT EXISTS idx_events_action_id_input ON events(event_action, id DESC)
WHERE event_action = 'device_snapshot'
  AND json_extract(event_data, '$.input_devices') IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_events_action_id_hw ON events(event_action, id DESC)
WHERE event_action = 'device_snapshot'
  AND json_extract(event_data, '$.memory.total_gb') IS NOT NULL;
