//! self_metric_minute 持久化（0.8.1 A1 自体遥测）：MetricPoint 出站 → 写入 → 查询。
//!
//! 表：0013_self_metric_minute（metric_id 键控分钟桶：PK (minute_bucket,
//! metric_id)，含 value/quality）。保留清理复用 cleanup_old_events 同事务
//! 骨架（retention 默认 0=永不删，显式开启才清）。

use super::Database;
use crate::metrics::MetricPoint;
use rusqlite::Connection;

/// 查询行（dash /api/self-metrics 直接 serde 序列化）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct SelfMetricMinuteRow {
    pub minute_bucket: i64,
    pub metric_id: String,
    pub value: f64,
    pub quality: String,
}

impl Database {
    /// MetricPoint 批量出站落库（writer 单事务；重投同键以最新采样为准）。
    /// 写入失败仅 log（诊断数据不阻塞采集）。
    pub fn insert_self_metric_points(&self, points: &[MetricPoint]) {
        if points.is_empty() {
            return;
        }
        self.with_writer(
            |conn| {
                if let Err(e) = conn.execute_batch("BEGIN IMMEDIATE") {
                    log::warn!("self_metric_minute 事务创建失败: {e}");
                    return;
                }
                let r = (|| -> rusqlite::Result<()> {
                    let mut stmt = conn.prepare_cached(
                    "INSERT INTO self_metric_minute (minute_bucket, metric_id, value, quality) \
                     VALUES (?1, ?2, ?3, ?4) \
                     ON CONFLICT(minute_bucket, metric_id) DO UPDATE SET \
                     value = excluded.value, quality = excluded.quality",
                )?;
                    for p in points {
                        let bucket = p.timestamp_utc_ms.div_euclid(60_000);
                        stmt.execute(rusqlite::params![
                            bucket,
                            p.metric_id,
                            p.value,
                            p.quality.tag(),
                        ])?;
                    }
                    Ok(())
                })();
                match r {
                    Ok(()) => {
                        if let Err(e) = conn.execute_batch("COMMIT") {
                            log::warn!("self_metric_minute 提交失败: {e}");
                        }
                    }
                    Err(e) => {
                        let _ = conn.execute_batch("ROLLBACK");
                        log::warn!("self_metric_minute 写入失败（诊断数据，不阻塞采集）: {e}");
                    }
                }
            },
            || log::warn!("self_metric_minute 写连接不可用，跳过本轮"),
        );
    }
}

/// 近 N 分钟自体指标行（minute_bucket >= now_ms/60000 - minutes）。
/// now_ms 注入（测试零真实时钟）。
pub fn query_self_metric_minute(
    conn: &Connection,
    minutes: u32,
    now_ms: i64,
) -> Vec<SelfMetricMinuteRow> {
    let cutoff = now_ms.div_euclid(60_000) - minutes as i64;
    let mut stmt = match conn.prepare(
        "SELECT minute_bucket, metric_id, value, quality FROM self_metric_minute \
         WHERE minute_bucket >= ?1 ORDER BY minute_bucket, metric_id",
    ) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let rows = stmt.query_map([cutoff], |r| {
        Ok(SelfMetricMinuteRow {
            minute_bucket: r.get(0)?,
            metric_id: r.get(1)?,
            value: r.get(2)?,
            quality: r.get(3)?,
        })
    });
    match rows {
        Ok(it) => it.filter_map(|r| r.ok()).collect(),
        Err(_) => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::{
        MetricPoint, MetricQuality, MetricScope, MetricSource, MetricUnit, ProcessIdentity,
        METRIC_SCHEMA_VERSION,
    };

    fn tmp_dir(tag: &str) -> std::path::PathBuf {
        let base =
            std::env::temp_dir().join(format!("ky-self-metrics-{tag}-{}", std::process::id()));
        let mut i = 0u32;
        loop {
            let p = base.with_extension(format!("{i}"));
            if !p.exists() {
                return p;
            }
            i += 1;
        }
    }

    fn point(now_ms: i64, metric_id: &str, value: f64) -> MetricPoint {
        MetricPoint {
            metric_id: metric_id.to_string(),
            source: MetricSource::WinApi,
            host_id: "H".into(),
            process_identity: ProcessIdentity::Id {
                pid: 1,
                exe_start_epoch: 0,
            },
            timestamp_utc_ms: now_ms,
            monotonic_ms: 0,
            interval_ms: 30_000,
            value,
            unit: MetricUnit::Percent,
            scope: MetricScope::Host,
            quality: MetricQuality::Confirmed,
            loss_count: None,
            coverage: None,
            schema_version: METRIC_SCHEMA_VERSION,
        }
    }

    #[test]
    fn self_metric_write_then_read_roundtrip_and_upsert_overwrites() {
        let dir = tmp_dir("rt");
        std::fs::create_dir_all(&dir).unwrap();
        let db = Database::open(dir.join("self.db").to_str().unwrap()).unwrap();
        // 同桶重投：以最新值为准（覆盖不累加）
        db.insert_self_metric_points(&[point(1000 * 60_000, "self.process.cpu_percent", 1.0)]);
        db.insert_self_metric_points(&[point(
            1000 * 60_000 + 30_000,
            "self.process.cpu_percent",
            7.5,
        )]);
        let rows = query_self_metric_minute(&db.reader(), 60, 1000 * 60_000);
        assert_eq!(rows.len(), 1, "同桶重投不得增行");
        assert!((rows[0].value - 7.5).abs() < 1e-9, "重投必须覆盖为最新值");
        assert_eq!(rows[0].quality, "confirmed");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn self_metric_query_window_is_bucket_inclusive() {
        let dir = tmp_dir("win");
        std::fs::create_dir_all(&dir).unwrap();
        let db = Database::open(dir.join("win.db").to_str().unwrap()).unwrap();
        // now_bucket=2000；窗口 5 分钟 → cutoff=1995：1994 出界，1995/2000 界内
        let pts = vec![
            point(1994 * 60_000, "self.process.old", 1.0),
            point(1995 * 60_000, "self.process.edge", 1.0),
            point(2000 * 60_000, "self.process.now", 1.0),
        ];
        db.insert_self_metric_points(&pts);
        let rows = query_self_metric_minute(&db.reader(), 5, 2000 * 60_000);
        let ids: Vec<&str> = rows
            .iter()
            .map(|r| r.metric_id.trim_start_matches("self.process."))
            .collect();
        assert_eq!(ids, vec!["edge", "now"], "窗口按 cutoff 桶含界");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
