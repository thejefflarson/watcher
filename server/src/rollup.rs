//! Downsampling: periodically fold raw `metrics` points into fixed-width time
//! buckets in `metric_rollups`. Charts read recent data from `metrics` and older
//! data from `metric_rollups`, so the raw points can be pruned aggressively
//! (see retention.rs) while history survives at reduced resolution.

use sqlx::PgPool;
use std::time::Duration;

/// Re-aggregate this many trailing buckets each pass so late-arriving points are
/// folded in. The upsert makes re-running a bucket idempotent. Kept small: each
/// pass hash-aggregates this much raw across every series, so at high ingest a
/// large lookback makes the sweep (and the Pi's DB) crawl.
const LOOKBACK_BUCKETS: f64 = 8.0;

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

/// Aggregate complete buckets in the trailing window into `metric_rollups` (the
/// attribute-collapsed rollup used by `metric_series`) and `metric_series_rollups`
/// (the per-series rollup used by the faceted views + expandable list). Only
/// buckets whose end is in the past are rolled up, so the current (still-filling)
/// bucket is left alone until it closes. Exposed for tests.
pub async fn rollup_once(pool: &PgPool, bucket_secs: i64) -> anyhow::Result<u64> {
    let width = bucket_secs as f64;
    // Window predicate shared by every pass: complete buckets in the lookback.
    let mut total = 0;

    // 1. Collapsed rollup (service+name), unchanged — feeds metric_series.
    total += sqlx::query(
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
    .await?
    .rows_affected();

    // 2. Per-series rollup for gauges/sums. max(value) doubles as the cumulative
    //    counter level the facet endpoint differences into a rate.
    total += sqlx::query(
        "INSERT INTO metric_series_rollups
            (bucket, name, series_key, attrs, service, kind, unit, is_monotonic,
             count, sum, min, max, avg)
         SELECT to_timestamp(floor(extract(epoch FROM time)::float8 / $1) * $1) AS bucket,
                name,
                md5(coalesce(service,'') || '|' || attributes::text) AS series_key,
                attributes,
                service,
                max(kind)         AS kind,
                max(unit)         AS unit,
                bool_or(is_monotonic) AS is_monotonic,
                count(*)          AS count,
                sum(value)        AS sum,
                min(value)        AS min,
                max(value)        AS max,
                avg(value)        AS avg
         FROM metrics
         WHERE kind IN ('gauge','sum') AND value IS NOT NULL
           AND time >= now() - make_interval(secs => $1 * $2)
           AND time <  to_timestamp(floor(extract(epoch FROM now())::float8 / $1) * $1)
         GROUP BY bucket, name, series_key, attributes, service
         ON CONFLICT (name, series_key, bucket) DO UPDATE
            SET count = EXCLUDED.count, sum = EXCLUDED.sum, min = EXCLUDED.min,
                max = EXCLUDED.max, avg = EXCLUDED.avg, kind = EXCLUDED.kind,
                unit = EXCLUDED.unit, is_monotonic = EXCLUDED.is_monotonic,
                attrs = EXCLUDED.attrs, service = EXCLUDED.service",
    )
    .bind(width)
    .bind(LOOKBACK_BUCKETS)
    .execute(pool)
    .await?
    .rows_affected();

    // 3. Per-series rollup for histograms: element-wise sum of bucket_counts so
    //    percentiles/heatmaps can be computed from the rollup.
    total += sqlx::query(
        "INSERT INTO metric_series_rollups
            (bucket, name, series_key, attrs, service, kind, unit,
             count, sum, min, max, avg, bucket_bounds, bucket_counts)
         SELECT to_timestamp(floor(extract(epoch FROM time)::float8 / $1) * $1) AS bucket,
                name,
                md5(coalesce(service,'') || '|' || attributes::text) AS series_key,
                attributes,
                service,
                'histogram'           AS kind,
                max(unit)             AS unit,
                sum(count)            AS count,
                sum(value)            AS sum,
                min(value)            AS min,
                max(value)            AS max,
                avg(value)            AS avg,
                min(bucket_bounds)            AS bucket_bounds,
                array_sum(bucket_counts)      AS bucket_counts
         FROM metrics
         WHERE kind = 'histogram' AND bucket_counts IS NOT NULL
           AND time >= now() - make_interval(secs => $1 * $2)
           AND time <  to_timestamp(floor(extract(epoch FROM now())::float8 / $1) * $1)
         GROUP BY bucket, name, series_key, attributes, service
         ON CONFLICT (name, series_key, bucket) DO UPDATE
            SET count = EXCLUDED.count, sum = EXCLUDED.sum, min = EXCLUDED.min,
                max = EXCLUDED.max, avg = EXCLUDED.avg, unit = EXCLUDED.unit,
                attrs = EXCLUDED.attrs, service = EXCLUDED.service,
                bucket_bounds = EXCLUDED.bucket_bounds,
                bucket_counts = EXCLUDED.bucket_counts",
    )
    .bind(width)
    .bind(LOOKBACK_BUCKETS)
    .execute(pool)
    .await?
    .rows_affected();

    Ok(total)
}
