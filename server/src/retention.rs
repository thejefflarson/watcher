//! Background retention: periodically delete telemetry older than the configured
//! window. Raw metric points are pruned on a shorter window than everything else
//! because `metric_rollups` (see rollup.rs) preserves their downsampled history.

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
    // History tables age out on the day window. (metric_rollups is legacy — no
    // longer written — but still pruned so it drains.)
    for (table, col) in [
        ("spans", "start_time"),
        ("logs", "time"),
        ("metric_rollups", "bucket"),
        ("metric_series_rollups", "bucket"),
    ] {
        let sql = format!("DELETE FROM {table} WHERE {col} < now() - make_interval(days => $1)");
        let r = sqlx::query(&sql).bind(days).execute(pool).await?;
        if r.rows_affected() > 0 {
            tracing::info!("retention: pruned {} rows from {table}", r.rows_affected());
            total += r.rows_affected();
        }
    }
    // Raw points: short hours-window cap (rollups hold the history).
    if raw_hours > 0 {
        let r = sqlx::query("DELETE FROM metrics WHERE time < now() - make_interval(hours => $1)")
            .bind(raw_hours)
            .execute(pool)
            .await?;
        if r.rows_affected() > 0 {
            tracing::info!("retention: pruned {} raw metric rows", r.rows_affected());
            total += r.rows_affected();
        }
    }
    Ok(total)
}
