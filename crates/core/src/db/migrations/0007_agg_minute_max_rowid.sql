-- 0007: agg_minute 增加 max_event_rowid 幂等防护列
-- 增量 upsert 记录已累计到的最大 events.id；重复投递（如分块回填重算
-- 已包含该事件后迟到的增量）被 WHERE 守卫跳过，保证同事件不重复累计。
ALTER TABLE agg_minute ADD COLUMN max_event_rowid INTEGER NOT NULL DEFAULT 0;
