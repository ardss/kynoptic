-- 0002: 开放 bucket 模型（schema-ddl-draft-v1）
-- bucket 注册表 + 事件类型注册表：新增监控器零 DDL 扩展。
CREATE TABLE IF NOT EXISTS schema_meta (
  key TEXT PRIMARY KEY,
  value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS buckets (
  id TEXT PRIMARY KEY,
  monitor TEXT NOT NULL,
  privacy_tier TEXT NOT NULL DEFAULT 'standard',
  enabled INTEGER NOT NULL DEFAULT 1,
  created_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS event_types (
  id TEXT PRIMARY KEY,
  bucket_id TEXT NOT NULL REFERENCES buckets(id),
  data_schema TEXT NOT NULL
);

-- 预聚合（分钟/日，供仪表盘与 MCP get_summary 低成本读取）
CREATE TABLE IF NOT EXISTS agg_minute (
  date TEXT NOT NULL, hour INTEGER NOT NULL, minute INTEGER NOT NULL,
  bucket_id TEXT NOT NULL,
  sum_value REAL, count_value INTEGER,
  PRIMARY KEY (date, hour, minute, bucket_id)
);
CREATE TABLE IF NOT EXISTS agg_daily (
  date TEXT NOT NULL,
  bucket_id TEXT NOT NULL,
  sum_value REAL, count_value INTEGER,
  PRIMARY KEY (date, bucket_id)
);

-- 当前状态视图数据（get_current_status 热路径）
CREATE TABLE IF NOT EXISTS current_state (
  key TEXT PRIMARY KEY,
  value TEXT NOT NULL,
  updated_at TEXT NOT NULL
);

-- pet 遗留表清理（新库不存在，幂等）
DROP TABLE IF EXISTS pet_signals;
DROP TABLE IF EXISTS pet_memory;
DROP TABLE IF EXISTS pet_state;
