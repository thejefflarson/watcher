//! Background retention: periodically delete telemetry older than the configured
//! window. Raw metric points are pruned on a shorter window than everything else
//! because `metric_rollups` (see rollup.rs) preserves their downsampled history.

use sqlx::PgPool;
use std::time::Duration;

/// Runs forever; ticks immediately, then hourly.
///
/// * `days` — retention for spans, logs, and metric rollups.
/// * `metrics_raw_days` — retention for raw metric points (typically smaller);
///   `<= 0` falls back to `days`.
pub async fn run(pool: PgPool, days: i32, metrics_raw_days: i32) {
    if days <= 0 {
        tracing::info!("retention disabled (WATCHER_RETENTION_DAYS <= 0)");
        return;
    }
    let mut interval = tokio::time::interval(Duration::from_secs(3600));
    loop {
        interval.tick().await;
        if let Err(e) = prune_once(&pool, days, metrics_raw_days).await {
            tracing::warn!("retention sweep failed: {e}");
        }
    }
}

/// A single retention sweep. Returns total rows deleted. Exposed for tests and
/// one-shot use; `run` calls it hourly.
pub async fn prune_once(pool: &PgPool, days: i32, metrics_raw_days: i32) -> anyhow::Result<u64> {
    let raw_days = if metrics_raw_days > 0 {
        metrics_raw_days.min(days)
    } else {
        days
    };
    let mut total = 0;
    for (table, col, window) in [
        ("spans", "start_time", days),
        ("logs", "time", days),
        ("metrics", "time", raw_days),
        ("metric_rollups", "bucket", days),
    ] {
        let sql = format!("DELETE FROM {table} WHERE {col} < now() - make_interval(days => $1)");
        let r = sqlx::query(&sql).bind(window).execute(pool).await?;
        if r.rows_affected() > 0 {
            tracing::info!("retention: pruned {} rows from {table}", r.rows_affected());
            total += r.rows_affected();
        }
    }
    Ok(total)
}
