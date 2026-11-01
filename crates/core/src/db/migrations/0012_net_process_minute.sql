-- 0012: 增强模式净指标分钟桶表（M2.4，ETW 内核网络 → MetricPoint 出站）
--
-- metric_id 键控分钟桶：PK (minute_bucket, pid, metric_id)——
--   minute_bucket  UTC epoch 毫秒 / 60000（M2.2 MinuteAggregator 桶口径）
--   pid            进程 id（进程身份 exe_start 见 M2.3 ProcessIdentityCache，
--                  不入表：桶粒度归属以 pid+metric_id 定键）
--   metric_id      三段命名（net.process.bytes_in / bytes_out，M1 规范）
--   bytes          传输层字节（口径注释：传输层字节≠应用载荷≠物理线路，
--                  与网卡接口计数的差异由接口计数对账，M2.2/M2.3 已入档）
--   loss_count     缓冲丢失事件数（NULL=无丢失观测，M2.2 LossStats）
--   coverage       采样覆盖率 0.0-1.0（NULL=无丢失观测）
--   quality        'confirmed' | 'incomplete'（M1 MetricQuality tag 子集，
--                  保留全名空间便于后续 estimated/unavailable 落表）
--
-- 写入：writer 管道同事务 upsert（重投以最新 drain 快照为准，覆盖不累加）。
-- 保留：显式 opt-in retention 后由 cleanup_old_events 与 events 同事务清理
-- （minute_bucket < cutoff_bucket）；默认 retention=0 永不删（铁律）。

CREATE TABLE IF NOT EXISTS net_process_minute (
    minute_bucket INTEGER NOT NULL,
    pid           INTEGER NOT NULL,
    metric_id     TEXT NOT NULL,
    bytes         INTEGER NOT NULL,
    loss_count    INTEGER,
    coverage      REAL,
    quality       TEXT NOT NULL,
    PRIMARY KEY (minute_bucket, pid, metric_id)
);

CREATE INDEX IF NOT EXISTS idx_net_process_minute_bucket
    ON net_process_minute(minute_bucket);
