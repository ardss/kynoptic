-- 0001: v0.1 采集器基础表（自上游 DigitalPulse 裁剪而来，pet 表已剥离）
CREATE TABLE IF NOT EXISTS events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    timestamp TEXT NOT NULL,
    event_type TEXT NOT NULL,
    event_action TEXT NOT NULL,
    event_data TEXT,
    app_name TEXT,
    window_title TEXT,
    session_id INTEGER
);

CREATE TABLE IF NOT EXISTS sessions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    start_time TEXT NOT NULL,
    end_time TEXT,
    total_events INTEGER DEFAULT 0,
    idle_seconds REAL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS metadata (
    key TEXT PRIMARY KEY,
    value TEXT
);

CREATE TABLE IF NOT EXISTS daily_agg (
    date TEXT PRIMARY KEY,
    keys INTEGER DEFAULT 0,
    clicks INTEGER DEFAULT 0,
    active_minutes INTEGER DEFAULT 0,
    apm_avg REAL DEFAULT 0
);

CREATE INDEX IF NOT EXISTS idx_events_timestamp ON events(timestamp);
CREATE INDEX IF NOT EXISTS idx_events_app ON events(app_name);
CREATE INDEX IF NOT EXISTS idx_events_session ON events(session_id);
-- 复合索引覆盖 80%+ 的查询模式（event_type + event_action + timestamp），
-- 前缀同时满足仅按 event_type 过滤的查询。
CREATE INDEX IF NOT EXISTS idx_events_type_action_ts ON events(event_type, event_action, timestamp);
