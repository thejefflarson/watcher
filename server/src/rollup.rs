//! Downsampling: periodically fold raw `metrics` points into fixed-width time
//! buckets in `metric_rollups`. Charts read recent data from `metrics` and older
//! data from `metric_rollups`, so the raw points can be pruned aggressively
//! (see retention.rs) while history survives at reduced resolution.

use sqlx::PgPool;
use std::time::Duration;

/// Re-aggregate this many trailing buckets each pass so late-arriving points are
/// folded in. The upsert makes re-running a bucket idempotent.
const LOOKBACK_BUCKETS: f64 = 12.0;

/// Runs forever; ticks immediately, then every `bucket_secs`.
pub async fn run(pool: PgPool, bucket_secs: i64) {
    if bucket_secs <= 0 {
        tracing::info!("rollups disabled (WATCHER_ROLLUP_BUCKET_SECS <= 0)");
        return;
    }
    let period = Duration::from_secs(bucket_secs.max(60) as u64);
    let mut interval = tokio::time::interval(period);
    loop {
        interval.tick().await;
        match rollup_once(&pool, bucket_secs).await {
            Ok(n) if n > 0 => tracing::info!("rollup: wrote {n} metric buckets"),
            Ok(_) => {}
            Err(e) => tracing::warn!("rollup sweep failed: {e}"),
        }
    }
}

/// Aggregate complete buckets in the trailing window into `metric_rollups`.
/// Only buckets whose end is in the past are rolled up, so the current
/// (still-filling) bucket is left alone until it closes. Exposed for tests.
pub async fn rollup_once(pool: &PgPool, bucket_secs: i64) -> anyhow::Result<u64> {
    let width = bucket_secs as f64;
    let res = sqlx::query(
        "INSERT INTO metric_rollups (bucket, service, name, kind, unit, count, sum, min, max, avg)
         SELECT to_timestamp(floor(extract(epoch FROM time)::float8 / $1) * $1) AS bucket,
                service,
                name,
                max(kind)  AS kind,
                max(unit)  AS unit,
                count(*)   AS count,
                sum(value) AS sum,
                min(value) AS min,
                max(value) AS max,
                avg(value) AS avg
         FROM metrics
         WHERE value IS NOT NULL
           AND time >= now() - make_interval(secs => $1 * $2)
           AND time <  to_timestamp(floor(extract(epoch FROM now())::float8 / $1) * $1)
         GROUP BY bucket, service, name
         ON CONFLICT (name, service_key, bucket) DO UPDATE
            SET count = EXCLUDED.count,
                sum   = EXCLUDED.sum,
                min   = EXCLUDED.min,
                max   = EXCLUDED.max,
                avg   = EXCLUDED.avg,
                kind  = EXCLUDED.kind,
                unit  = EXCLUDED.unit",
    )
    .bind(width)
    .bind(LOOKBACK_BUCKETS)
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}
