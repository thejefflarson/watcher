//! Background retention: periodically delete telemetry older than the configured
//! window. Raw metric points are pruned on a shorter window than everything else
//! because `metric_series_rollups` (maintained on ingest) preserves their
//! downsampled per-series history.
//!
//! Spans, logs, and metric rollups can each be given their own window:
//! [`Windows`] carries an optional per-table override, declared via
//! `WATCHER_RETENTION_SPANS_DAYS` / `WATCHER_RETENTION_LOGS_DAYS` /
//! `WATCHER_RETENTION_METRICS_DAYS`. A table with no override falls back to the
//! existing global `WATCHER_RETENTION_DAYS` — an all-omitted config is exactly
//! today's single window, so this is a no-op for anyone who doesn't set the new
//! vars. This is deliberately per-*table*, not per-service.
//!
//! EVERY delete here is `ctid`-batched via [`prune_batched`]. That used to be
//! true only of `prune_raw_metrics`, on the assumption that a whole-table delete
//! was small enough to land inside the statement timeout. It is not: once a table
//! accumulates a backlog, the single DELETE times out, rolls back completely, and
//! the backlog it failed to clear makes the next attempt slower — a stall that
//! never recovers on its own.

use sqlx::PgPool;
use std::time::Duration;

/// Optional per-table retention overrides, in days. A `None` field falls back to
/// the global default passed to [`run`] / [`prune_once`]. An explicit `Some(n)`
/// with `n <= 0` disables pruning for just that table.
#[derive(Debug, Clone, Copy, Default)]
pub struct Windows {
    pub spans_days: Option<i32>,
    pub logs_days: Option<i32>,
    pub metrics_days: Option<i32>,
}

/// Runs forever; ticks immediately, then hourly.
///
/// * `days` — the global default window, used by any table without its own
///   override in `windows`.
/// * `raw_hours` — retention for raw metric points (a short full-resolution
///   window; rollups carry the longer history). `<= 0` disables raw pruning.
/// * `windows` — optional per-table overrides of `days` (see module docs).
pub async fn run(pool: PgPool, days: i32, raw_hours: i32, windows: Windows) {
    if days <= 0 {
        tracing::info!("retention disabled (WATCHER_RETENTION_DAYS <= 0)");
        return;
    }
    let mut interval = tokio::time::interval(Duration::from_secs(3600));
    loop {
        interval.tick().await;
        if let Err(e) = prune_once(&pool, days, raw_hours, windows).await {
            tracing::warn!("retention sweep failed: {e}");
        }
    }
}

/// A single retention sweep. Returns total rows deleted. Exposed for tests and
/// one-shot use; `run` calls it hourly.
pub async fn prune_once(
    pool: &PgPool,
    days: i32,
    raw_hours: i32,
    windows: Windows,
) -> anyhow::Result<u64> {
    let mut total = 0;
    // Tables that failed this sweep. Collected rather than propagated with `?`
    // so ONE bad table cannot starve the ones after it — see below.
    let mut failed: Vec<&str> = Vec::new();
    // History tables age out on their own window (falling back to `days`).
    for (table, col, override_days) in [
        ("spans", "start_time", windows.spans_days),
        ("logs", "time", windows.logs_days),
        ("metric_series_rollups", "bucket", windows.metrics_days),
    ] {
        let table_days = override_days.unwrap_or(days);
        if table_days <= 0 {
            continue;
        }
        match prune_batched(pool, table, col, "days", table_days, HISTORY_PRUNE_BATCH).await {
            Ok(n) => {
                if n > 0 {
                    tracing::info!("retention: pruned {n} rows from {table}");
                    total += n;
                }
            }
            // Keep going. Previously this was `?`, so the FIRST failing table
            // aborted the sweep and every table after it in this list was never
            // pruned at all. Observed 2026-08-25: the unbatched `logs` delete
            // timed out every hour, and because `metric_series_rollups` comes
            // after it, that table was never swept once and reached 20 GB.
            Err(e) => {
                tracing::warn!("retention: {table} sweep failed: {e}");
                failed.push(table);
            }
        }
    }
    // Raw points: short hours-window cap (rollups hold the history).
    if raw_hours > 0 {
        match prune_raw_metrics(pool, raw_hours, RAW_PRUNE_BATCH).await {
            Ok(pruned) => {
                if pruned > 0 {
                    tracing::info!("retention: pruned {pruned} raw metric rows");
                    total += pruned;
                }
            }
            Err(e) => {
                tracing::warn!("retention: metrics sweep failed: {e}");
                failed.push("metrics");
            }
        }
    }
    // Only a sweep where EVERY table succeeded counts. A partial sweep leaves
    // some table growing, which is precisely the condition /healthz must keep
    // reporting as stalled.
    if !failed.is_empty() {
        anyhow::bail!("retention incomplete; failed tables: {}", failed.join(", "));
    }
    // Record the successful sweep so self-telemetry can surface its recency and
    // /healthz can flag a stall (a silent retention stall is exactly what let the
    // metrics table grow to tens of GB un-paged).
    crate::selfmon::record_retention_success(total);
    Ok(total)
}

/// Rows deleted per batch for the history tables.
///
/// Deliberately smaller than [`RAW_PRUNE_BATCH`]: these tables carry far more
/// indexes than raw `metrics` — `logs` alone has five, including a GIN index on
/// `attributes` — and every deleted row must be removed from each one, so an
/// equal-sized batch costs several times as much.
const HISTORY_PRUNE_BATCH: i64 = 10_000;

/// Delete rows older than `amount` `unit`s from `table`, in batches of `batch`.
///
/// WHY BATCHING IS NOT OPTIONAL HERE. A single
/// `DELETE FROM <table> WHERE <col> < cutoff` over a large backlog exceeds the
/// connection's `statement_timeout`, and a cancelled DELETE **rolls back
/// entirely** — so the sweep deletes nothing, the backlog grows, and the next
/// sweep is slower still. It never recovers on its own.
///
/// That is not hypothetical: on 2026-08-25 `logs` held 11.5M rows past a 7-day
/// window, the hourly sweep hit its 60s timeout with `rows_affected=0` every
/// single time, and retention had NEVER completed a sweep in the life of the
/// process. Batching by `ctid` keeps each statement small enough to commit, so a
/// backlog drains across successive statements and each one is progress that
/// survives. In steady state the first batch is already short and the loop exits
/// after one pass.
/// `pub` so tests can drive it with a tiny `batch` and prove a backlog really
/// drains across statements — the property that was silently absent here.
pub async fn prune_batched(
    pool: &PgPool,
    table: &str,
    col: &str,
    unit: &str,
    amount: i32,
    batch: i64,
) -> anyhow::Result<u64> {
    let mut pruned = 0u64;
    loop {
        let sql = format!(
            "DELETE FROM {table} WHERE ctid IN (
                 SELECT ctid FROM {table}
                 WHERE {col} < now() - make_interval({unit} => $1)
                 LIMIT $2)"
        );
        // AssertSqlSafe: sqlx 0.9 requires dynamic SQL be audited. `table`, `col`
        // and `unit` come only from hardcoded call sites in this module, never
        // from config or request data, so there is no injection surface.
        let r = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(amount)
            .bind(batch)
            .execute(pool)
            .await?;
        let n = r.rows_affected();
        pruned += n;
        // A short batch means nothing older than the cutoff remains.
        if (n as i64) < batch {
            break;
        }
    }
    Ok(pruned)
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
    prune_batched(pool, "metrics", "time", "hours", raw_hours, batch).await
}
