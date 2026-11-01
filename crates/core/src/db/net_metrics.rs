//! net_process_minute 持久化（M2.4）：MetricPoint 出站 → writer 管道 → 查询。
//!
//! 表：0012_net_process_minute（metric_id 键控分钟桶：PK (minute_bucket, pid,
//! metric_id)，含 loss_count/coverage/quality 列）。保留清理复用 S1.1 的
//! cleanup_old_events 同事务骨架（retention 默认 0=永不删，显式开启才清）。

use super::Database;
use crate::metrics::MetricPoint;
use rusqlite::Connection;

/// 查询行（dash /api/net-process 直接 serde 序列化）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct NetProcessMinuteRow {
    pub minute_bucket: i64,
    pub pid: u32,
    pub metric_id: String,
    pub bytes: i64,
    pub loss_count: Option<i64>,
    pub coverage: Option<f64>,
    pub quality: String,
}

impl Database {
    /// MetricPoint 批量出站落库（writer 单事务；重投同键以最新值为准——
    /// drain 快照是权威口径，覆盖不累加）。写入失败仅 log（派生数据可由
    /// 上游重投/重建，不阻塞采集）。
    pub fn insert_net_process_minute_points(&self, points: &[MetricPoint]) {
        if points.is_empty() {
            return;
        }
        self.with_writer(
            |conn| {
                if let Err(e) = conn.execute_batch("BEGIN IMMEDIATE") {
                    log::warn!("net_process_minute 事务创建失败: {e}");
                    return;
                }
                let r = (|| -> rusqlite::Result<()> {
                    let mut stmt = conn.prepare_cached(
                        "INSERT INTO net_process_minute                          (minute_bucket, pid, metric_id, bytes, loss_count, coverage, quality)                          VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)                          ON CONFLICT(minute_bucket, pid, metric_id) DO UPDATE SET                          bytes = excluded.bytes, loss_count = excluded.loss_count,                          coverage = excluded.coverage, quality = excluded.quality",
                    )?;
                    for p in points {
                        let bucket = p.timestamp_utc_ms.div_euclid(60_000);
                        let pid = match p.process_identity {
                            crate::metrics::ProcessIdentity::Id { pid, .. } => pid,
                            crate::metrics::ProcessIdentity::None => continue,
                        };
                        stmt.execute(rusqlite::params![
                            bucket,
                            pid,
                            p.metric_id,
                            p.value as i64,
                            p.loss_count.map(|l| l as i64),
                            p.coverage,
                            p.quality.tag(),
                        ])?;
                    }
                    Ok(())
                })();
                match r {
                    Ok(()) => {
                        if let Err(e) = conn.execute_batch("COMMIT") {
                            log::warn!("net_process_minute 提交失败: {e}");
                            let _ = conn.execute_batch("ROLLBACK");
                        }
                    }
                    Err(e) => {
                        log::warn!("net_process_minute 写入失败: {e}");
                        let _ = conn.execute_batch("ROLLBACK");
                    }
                }
            },
            || (),
        );
    }
}

/// 窗口查询：最近 `minutes` 个分钟桶（now_ms 注入，禁真实时钟），
/// 按桶/进程/指标排序。只读走 reader 池连接。
pub fn query_net_process_minute(
    conn: &Connection,
    minutes: u32,
    now_ms: i64,
) -> Vec<NetProcessMinuteRow> {
    let cutoff_bucket = now_ms.div_euclid(60_000) - minutes as i64;
    let mut stmt = match conn.prepare(
        "SELECT minute_bucket, pid, metric_id, bytes, loss_count, coverage, quality          FROM net_process_minute WHERE minute_bucket > ?1          ORDER BY minute_bucket, pid, metric_id",
    ) {
        Ok(s) => s,
        Err(e) => {
            log::warn!("net_process_minute 查询失败: {e}");
            return Vec::new();
        }
    };
    let rows = stmt.query_map([cutoff_bucket], |r| {
        Ok(NetProcessMinuteRow {
            minute_bucket: r.get(0)?,
            pid: r.get(1)?,
            metric_id: r.get(2)?,
            bytes: r.get(3)?,
            loss_count: r.get(4)?,
            coverage: r.get(5)?,
            quality: r.get(6)?,
        })
    });
    match rows {
        Ok(rows) => rows.collect::<Result<Vec<_>, _>>().unwrap_or_default(),
        Err(e) => {
            log::warn!("net_process_minute 读失败: {e}");
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::{MetricPoint, MetricQuality, MetricSource, MetricUnit};

    /// 临时库目录（pid+seq 纯计数，禁真实时钟；与 events.rs tests 同款）
    fn tmp_dir(tag: &str) -> std::path::PathBuf {
        static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let mut seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        loop {
            let dir = std::env::temp_dir().join(format!("kyn-{tag}-{}-{seq}", std::process::id()));
            if !dir.exists() {
                return dir;
            }
            seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// 造一个 M2.2 形状的出站点（桶起点时间戳、interval 60s）
    fn point(bucket: i64, pid: u32, metric_id: &str, bytes: u64, lost: Option<u64>) -> MetricPoint {
        let quality = if lost.is_some() {
            MetricQuality::Incomplete
        } else {
            MetricQuality::Confirmed
        };
        MetricPoint {
            metric_id: metric_id.to_string(),
            source: MetricSource::Etw,
            host_id: "H".to_string(),
            process_identity: crate::metrics::ProcessIdentity::Id {
                pid,
                exe_start_epoch: 100,
            },
            timestamp_utc_ms: bucket * 60_000,
            monotonic_ms: 0,
            interval_ms: 60_000,
            value: bytes as f64,
            unit: MetricUnit::Bytes,
            scope: crate::metrics::MetricScope::Process(crate::metrics::ProcessIdentity::Id {
                pid,
                exe_start_epoch: 100,
            }),
            quality,
            loss_count: lost,
            coverage: lost.map(|_| 0.5),
            schema_version: crate::metrics::METRIC_SCHEMA_VERSION,
        }
    }

    fn net_rows(db: &Database) -> i64 {
        db.reader()
            .query_row("SELECT COUNT(*) FROM net_process_minute", [], |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn m24_write_then_read_roundtrip() {
        let dir = tmp_dir("m24rt");
        std::fs::create_dir_all(&dir).unwrap();
        let db = Database::open(dir.join("m24.db").to_str().unwrap()).unwrap();

        let pts = vec![
            point(100, 7, "net.process.bytes_in", 600, None),
            point(100, 7, "net.process.bytes_out", 50, None),
            point(101, 7, "net.process.bytes_in", 999, Some(3)),
        ];
        db.insert_net_process_minute_points(&pts);

        // 按窗口读回（now=102 号桶末，窗口 60 分钟覆盖全部）
        let now_ms = 102 * 60_000;
        let rows = query_net_process_minute(&db.reader(), 60, now_ms);
        assert_eq!(rows.len(), 3, "三条桶记录全读回");
        let row = rows
            .iter()
            .find(|r| r.minute_bucket == 100 && r.metric_id == "net.process.bytes_in")
            .expect("in 桶必须有行");
        assert_eq!(row.pid, 7);
        assert_eq!(row.bytes, 600);
        assert_eq!(row.loss_count, None);
        assert_eq!(row.coverage, None);
        assert_eq!(row.quality, "confirmed");
        let lost = rows
            .iter()
            .find(|r| r.loss_count.is_some())
            .expect("丢失行必须有");
        assert_eq!(lost.loss_count, Some(3));
        assert_eq!(lost.quality, "incomplete");
        assert!((lost.coverage.unwrap() - 0.5).abs() < 1e-9);

        // 窗口收窄到只含 101 号桶（minutes=1 → cutoff=101，严格 >101：
        // 桶 100/101 均不可见、桶 101 在 minutes=2 时可见）
        let rows2 = query_net_process_minute(&db.reader(), 2, 102 * 60_000);
        assert!(
            rows2.iter().all(|r| r.minute_bucket > 100),
            "桶 100 必须出窗"
        );
        assert_eq!(rows2.len(), 1, "窗口外旧桶不得读出");

        db.mark_stopping();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn m24_upsert_replaces_not_duplicates() {
        let dir = tmp_dir("m24up");
        std::fs::create_dir_all(&dir).unwrap();
        let db = Database::open(dir.join("m24.db").to_str().unwrap()).unwrap();
        db.insert_net_process_minute_points(&[point(100, 7, "net.process.bytes_in", 100, None)]);
        // 同键重投（drain 权威快照语义）：覆盖而非累加/重复
        db.insert_net_process_minute_points(&[point(100, 7, "net.process.bytes_in", 777, None)]);
        assert_eq!(net_rows(&db), 1, "同键必须覆盖为一行");
        let rows = query_net_process_minute(&db.reader(), 60, 102 * 60_000);
        assert_eq!(rows[0].bytes, 777, "重投以最新值为准");
        db.mark_stopping();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn m24_retention_on_clears_expired_off_is_noop() {
        // 开启（retention=30）：旧桶清、新桶留。cleanup 内部以真实 now 定
        // 截止桶，桶号必须相对 now 注入（数据侧相对注入，断言不依赖墙钟值）
        let now_bucket = chrono::Utc::now().timestamp_millis().div_euclid(60_000);
        let dir = tmp_dir("m24ret");
        std::fs::create_dir_all(&dir).unwrap();
        let mut db = Database::open(dir.join("on.db").to_str().unwrap()).unwrap();
        let old_bucket = now_bucket - 60 * 24 * 40; // 40 天前
        let new_bucket = now_bucket - 10; // 10 分钟前（保留期内）
        db.insert_net_process_minute_points(&[
            point(old_bucket, 7, "net.process.bytes_in", 1, None),
            point(new_bucket, 7, "net.process.bytes_in", 2, None),
        ]);
        db.retention_days = 30; // 测试注入：db 模块私有字段（同模块子模块可写）
        db.cleanup_old_events();
        let rows = query_net_process_minute(&db.reader(), 100_000, new_bucket * 60_000);
        assert_eq!(rows.len(), 1, "过期桶必须被清");
        assert_eq!(rows[0].minute_bucket, new_bucket);
        db.mark_stopping();
        let _ = std::fs::remove_dir_all(&dir);

        // 关闭（retention=0 默认）：零清理（铁律，S1.1 守卫同骨架）
        let dir = tmp_dir("m24retoff");
        std::fs::create_dir_all(&dir).unwrap();
        let db = Database::open(dir.join("off.db").to_str().unwrap()).unwrap();
        db.insert_net_process_minute_points(&[
            point(
                now_bucket - 60 * 24 * 40,
                7,
                "net.process.bytes_in",
                1,
                None,
            ),
            point(now_bucket - 10, 7, "net.process.bytes_in", 2, None),
        ]);
        let deleted = db.cleanup_old_events();
        assert_eq!(deleted, 0, "retention=0 时 cleanup 不得删任何行");
        assert_eq!(net_rows(&db), 2, "retention=0 净指标表零清理");
        db.mark_stopping();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
