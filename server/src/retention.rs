//! Background retention: periodically delete telemetry older than the configured
//! window. Raw metric points are pruned on a shorter window than everything else
//! because `metric_series_rollups` (maintained on ingest) preserves their
//! downsampled per-series history.

use sqlx::PgPool;
use std::time::Duration;

/// Runs forever; ticks immediately, then hourly.
///
/// * `days` — retention for spans, logs, and the per-series metric rollups.
/// * `raw_hours` — retention for raw metric points (a short full-resolution
///   window; rollups carry the longer history). `<= 0` disables raw pruning.
pub async fn run(pool: PgPool, days: i32, raw_hours: i32) {
    if days <= 0 {
        tracing::info!("retention disabled (WATCHER_RETENTION_DAYS <= 0)");
        return;
    }
    let mut interval = tokio::time::interval(Duration::from_secs(3600));
    loop {
        interval.tick().await;
        if let Err(e) = prune_once(&pool, days, raw_hours).await {
            tracing::warn!("retention sweep failed: {e}");
        }
    }
}

/// A single retention sweep. Returns total rows deleted. Exposed for tests and
/// one-shot use; `run` calls it hourly.
pub async fn prune_once(pool: &PgPool, days: i32, raw_hours: i32) -> anyhow::Result<u64> {
    let mut total = 0;
    // History tables age out on the day window.
    for (table, col) in [
        ("spans", "start_time"),
        ("logs", "time"),
        ("metric_series_rollups", "bucket"),
    ] {
        let sql = format!("DELETE FROM {table} WHERE {col} < now() - make_interval(days => $1)");
        // AssertSqlSafe: sqlx 0.9 requires dynamic SQL be audited; table/col come
        // only from the hardcoded list above, so there's no injection surface.
        let r = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(days)
            .execute(pool)
            .await?;
        if r.rows_affected() > 0 {
            tracing::info!("retention: pruned {} rows from {table}", r.rows_affected());
            total += r.rows_affected();
        }
    }
    // Raw points: short hours-window cap (rollups hold the history).
    if raw_hours > 0 {
        let pruned = prune_raw_metrics(pool, raw_hours, RAW_PRUNE_BATCH).await?;
        if pruned > 0 {
            tracing::info!("retention: pruned {pruned} raw metric rows");
            total += pruned;
        }
    }
    Ok(total)
}

/// Rows deleted per raw-metrics batch. Bounded so a large backlog drains across
/// many small statements instead of one huge DELETE.
const RAW_PRUNE_BATCH: i64 = 50_000;

/// Delete raw metric points older than `raw_hours` in batches of `batch` rows.
///
/// The raw `metrics` table is the highest-volume table, and a single
/// `DELETE ... WHERE time < cutoff` over a large backlog can exceed the
/// connection's `statement_timeout` and roll back — every sweep — leaving raw
/// metrics to grow unbounded (this is exactly how the table once reached 35 GB).
/// Batching by `ctid` keeps each statement small and lets a backlog drain over
/// successive iterations; in steady state the first batch is already partial and
/// the loop exits after one pass. Returns the total rows deleted.
pub async fn prune_raw_metrics(pool: &PgPool, raw_hours: i32, batch: i64) -> anyhow::Result<u64> {
    let mut pruned = 0u64;
    loop {
        let r = sqlx::query(
            "DELETE FROM metrics WHERE ctid IN (
                 SELECT ctid FROM metrics
                 WHERE time < now() - make_interval(hours => $1)
                 LIMIT $2)",
        )
        .bind(raw_hours)
        .bind(batch)
        .execute(pool)
        .await?;
        let n = r.rows_affected();
        pruned += n;
        // A short batch means no rows older than the cutoff remain.
        if (n as i64) < batch {
            break;
        }
    }
    Ok(pruned)
}
