-- 0013: 自体遥测分钟桶表（0.8.1 A1：采集进程自身的 CPU/内存/线程/句柄）
--
-- 动机（2026-11-02 审查）：kynoptic 自身的资源消耗此前只能靠 process_snapshot
-- 的全机 Top5 旁路采样偶尔看到——10 月内存 194→350MB 线性爬升一个月才被发现。
-- 本表把「自己监控自己」变成连续记录：采集循环每分钟把自身进程指标按 M1
-- MetricPoint 契约落一行，内存再爬升第一天就能在曲线上看见。
--
-- metric_id 键控分钟桶：PK (minute_bucket, metric_id)——
--   minute_bucket  UTC epoch 毫秒 / 60000（与 net_process_minute 同桶口径）
--   metric_id      self.process.cpu_percent / working_set_mb / threads / handles
--   value          实测值（cpu=差分归一 0-100，working_set_mb=MiB，其余计数）
--   quality        'confirmed'（首拍 CPU 无差分基线时不出行，不出 estimated）
--
-- 写入：SelfMetrics 线程每轮 upsert（重投以最新采样为准）。
-- 保留：显式 opt-in retention 后与 events 同事务清理（minute_bucket < cutoff）；
-- 默认 retention=0 永不删（铁律）。

CREATE TABLE IF NOT EXISTS self_metric_minute (
    minute_bucket INTEGER NOT NULL,
    metric_id     TEXT NOT NULL,
    value         REAL NOT NULL,
    quality       TEXT NOT NULL,
    PRIMARY KEY (minute_bucket, metric_id)
);

CREATE INDEX IF NOT EXISTS idx_self_metric_minute_bucket
    ON self_metric_minute(minute_bucket);
