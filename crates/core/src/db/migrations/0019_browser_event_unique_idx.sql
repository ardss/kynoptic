-- 0019: browser-host 事件去重键部分唯一索引（独立复审 2026-11-11 medium）
--
-- browser-host 的 ingest_ext_event 此前是「先 SELECT COUNT 再 INSERT」两步
-- 去重，去重键（json_extract 出的 event_id / session_id）上没有唯一索引
-- ——Chrome sendNativeMessage 每次调用拉起一个独立 host 进程，同键并发重发
-- 可两进程同时通过 COUNT=0 检查后各插一行，幂等性在并发下被破坏。
-- 本迁移补两个表达式部分唯一索引（只加索引，不动既有数据，append-only
-- 铁律不受影响），摄取侧随之改 INSERT OR IGNORE 单步写入：
--
-- - window/tab_change：键 = $.event_id（扩展生成的事件 id）；
-- - ai_session/ai_run：键 = $.event_data.session_id（浏览器侧 AI 会话按
--   会话去重的既有口径，原样固化）。
--
-- 若历史库已有并发竞争留下的重复行，建唯一索引会失败并**硬失败**迁移
-- （宁可不升级也不静默掩盖脏数据）；清库前先人工核查重复行。

CREATE UNIQUE INDEX IF NOT EXISTS idx_events_browser_tab_event_id
    ON events (json_extract(event_data, '$.event_id'))
    WHERE event_type = 'window'
      AND event_action = 'tab_change'
      AND json_extract(event_data, '$.event_id') IS NOT NULL;

CREATE UNIQUE INDEX IF NOT EXISTS idx_events_browser_ai_session_id
    ON events (json_extract(event_data, '$.event_data.session_id'))
    WHERE event_type = 'ai_session'
      AND event_action = 'ai_run'
      AND json_extract(event_data, '$.event_data.session_id') IS NOT NULL;
