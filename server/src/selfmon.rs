//! watcher self-monitoring (ADR 0014): operational gauges + counters about
//! watcher's own health, plus the deep `/healthz` computation.
//!
//! The ops metrics are handed straight to [`crate::otlp::store_metrics`] rather
//! than exported over the network to `OTEL_EXPORTER_OTLP_ENDPOINT` (which the
//! trace self-export uses and which may point at an *external* collector). That
//! guarantees they reach watcher's *own* Postgres — the only place its UI reads
//! and its alert evaluator queries — regardless of where traces are shipped, and
//! it needs no network hop, no self-scrape loop, and no metrics SDK/exporter
//! wiring. The points are tagged `service.name=watcher`, so they ride the metrics
//! UI and are alertable through the normal alert-rule surface.

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::Utc;
use opentelemetry_proto::tonic::{
    collector::metrics::v1::ExportMetricsServiceRequest,
    common::v1::{any_value, AnyValue, KeyValue},
    metrics::v1::{
        metric, number_data_point, Gauge, Metric, NumberDataPoint, ResourceMetrics, ScopeMetrics,
        Sum,
    },
    resource::v1::Resource,
};
use sqlx::PgPool;

// ---------------------------------------------------------------------------
// Ingest + drop counters — incremented from the ingest path (otlp.rs).
// ---------------------------------------------------------------------------

/// Rows successfully stored, by signal — the ingest-throughput counters. Emitted
/// as monotonic sums, so the metrics UI rates them into per-second throughput.
pub static SPANS_INGESTED: AtomicU64 = AtomicU64::new(0);
pub static LOGS_INGESTED: AtomicU64 = AtomicU64::new(0);
pub static METRIC_POINTS_INGESTED: AtomicU64 = AtomicU64::new(0);

/// Dropped-point counters, one per existing drop path:
/// * `DROP_CAP` — requests truncated by `MAX_POINTS_PER_REQUEST`;
/// * `DROP_DECODE` — requests rejected by a protobuf decode error;
/// * `DROP_INSERT` — rows whose insert failed (a failed batch counts its size).
pub static DROP_CAP: AtomicU64 = AtomicU64::new(0);
pub static DROP_DECODE: AtomicU64 = AtomicU64::new(0);
pub static DROP_INSERT: AtomicU64 = AtomicU64::new(0);

// ---------------------------------------------------------------------------
// Retention last-run state (in-process; no new table).
// ---------------------------------------------------------------------------

/// Unix seconds of the last successful retention sweep, `0` = none yet.
static RETENTION_LAST_SUCCESS_UNIX: AtomicI64 = AtomicI64::new(0);
/// Rows deleted by that sweep.
static RETENTION_LAST_SUCCESS_ROWS: AtomicU64 = AtomicU64::new(0);

/// Record a successful retention sweep — called from `retention::prune_once`.
pub fn record_retention_success(rows: u64) {
    RETENTION_LAST_SUCCESS_UNIX.store(Utc::now().timestamp(), Ordering::Relaxed);
    RETENTION_LAST_SUCCESS_ROWS.store(rows, Ordering::Relaxed);
}

/// Unix seconds of the process start, captured once. Used as the retention
/// staleness reference before the first sweep completes, so a retention loop that
/// never succeeds still trips the health check after `threshold` seconds of
/// uptime rather than looking healthy forever.
fn process_started_unix() -> i64 {
    static STARTED: OnceLock<i64> = OnceLock::new();
    *STARTED.get_or_init(|| Utc::now().timestamp())
}

/// Capture the process start time. Call once at startup so uptime is measured
/// from boot rather than from the first `/healthz` hit.
pub fn mark_started() {
    let _ = process_started_unix();
}

// ---------------------------------------------------------------------------
// Config / guard.
// ---------------------------------------------------------------------------

/// Self-telemetry is on unless `WATCHER_SELF_TELEMETRY` is `0`/`false`/`off` —
/// the same switch that guards the trace self-export in `main.rs`.
pub fn enabled() -> bool {
    !std::env::var("WATCHER_SELF_TELEMETRY")
        .map(|v| matches!(v.as_str(), "0" | "false" | "off"))
        .unwrap_or(false)
}

fn interval_secs() -> u64 {
    env_u64("WATCHER_SELF_TELEMETRY_INTERVAL_SECS", 60).max(5)
}

/// Retention is considered stalled once the last successful sweep (or process
/// start, before the first one) is older than this. Default 2h — the sweep runs
/// hourly, so two missed cycles trips it. Raise it if you run a raw-retention
/// window longer than the default.
fn max_retention_age_secs() -> i64 {
    env_u64("WATCHER_HEALTHZ_MAX_RETENTION_AGE_SECS", 7200) as i64
}

fn service_name() -> String {
    std::env::var("OTEL_SERVICE_NAME").unwrap_or_else(|_| "watcher".to_string())
}

fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|&v| v > 0)
        .unwrap_or(default)
}

// ---------------------------------------------------------------------------
// Deep /healthz computation.
// ---------------------------------------------------------------------------

/// Pure retention-stall decision, factored out so it's testable without a DB or a
/// clock. `reference` is the last successful sweep, or process start when none
/// has happened yet; stalled once it's older than `threshold_secs`.
pub fn retention_stalled(
    last_success_unix: i64,
    started_unix: i64,
    now_unix: i64,
    threshold_secs: i64,
) -> bool {
    let reference = if last_success_unix > 0 {
        last_success_unix
    } else {
        started_unix
    };
    now_unix.saturating_sub(reference) > threshold_secs
}

/// A snapshot of the deep health check.
pub struct Health {
    pub db_ok: bool,
    pub retention_stalled: bool,
    /// Age of the last successful retention sweep, or `None` if none has run yet.
    pub retention_last_success_age_secs: Option<i64>,
}

impl Health {
    /// Ready to serve traffic: DB reachable AND retention not stalled.
    pub fn healthy(&self) -> bool {
        self.db_ok && !self.retention_stalled
    }
}

/// Compute the deep health check: probe the DB and read the in-process retention
/// state.
pub async fn health(pool: &PgPool) -> Health {
    let db_ok = sqlx::query_scalar::<_, i32>("SELECT 1")
        .fetch_one(pool)
        .await
        .is_ok();
    let now = Utc::now().timestamp();
    let last = RETENTION_LAST_SUCCESS_UNIX.load(Ordering::Relaxed);
    Health {
        db_ok,
        retention_stalled: retention_stalled(
            last,
            process_started_unix(),
            now,
            max_retention_age_secs(),
        ),
        retention_last_success_age_secs: (last > 0).then_some(now - last),
    }
}

// ---------------------------------------------------------------------------
// Emit loop.
// ---------------------------------------------------------------------------

/// Run forever: on a timer, snapshot watcher's own ops metrics and store them via
/// the normal ingest path. The first tick fires immediately, so the `watcher_*`
/// series appear within one interval of startup.
pub async fn run(pool: PgPool) {
    let mut ticker = tokio::time::interval(Duration::from_secs(interval_secs()));
    loop {
        ticker.tick().await;
        if let Err(e) = emit_once(&pool).await {
            tracing::warn!("self-telemetry: metric emit failed: {e}");
        }
    }
}

/// Gather one snapshot of ops metrics and store it. Exposed for tests.
pub async fn emit_once(pool: &PgPool) -> anyhow::Result<()> {
    let metrics = collect_metrics(pool).await?;
    if metrics.is_empty() {
        return Ok(());
    }
    let req = ExportMetricsServiceRequest {
        resource_metrics: vec![ResourceMetrics {
            resource: Some(Resource {
                attributes: vec![str_kv("service.name", &service_name())],
                ..Default::default()
            }),
            scope_metrics: vec![ScopeMetrics {
                metrics,
                ..Default::default()
            }],
            ..Default::default()
        }],
    };
    crate::otlp::store_metrics(pool, req).await;
    Ok(())
}

/// The telemetry tables whose on-disk size is tracked (a fixed allowlist).
const TRACKED_TABLES: [&str; 6] = [
    "spans",
    "logs",
    "metrics",
    "metric_series_rollups",
    "alert_events",
    "alert_rules",
];

/// The table the index-only-scan / visibility-map health canary watches. The
/// covering index only produces index-only scans while this high-churn
/// table's visibility map stays current; if autovacuum falls behind, scans
/// silently degrade to heap fetches and the same class of latency regression
/// returns with no signal until dashboards get slow.
const ROLLUP_TABLE: &str = "metric_series_rollups";

/// Build the `watcher_*` metric points from cheap catalog/aggregate queries plus
/// the in-process counters.
async fn collect_metrics(pool: &PgPool) -> anyhow::Result<Vec<Metric>> {
    let nanos = now_nanos();

    // Per-table on-disk bytes (heap + indexes + toast) in one catalog lookup —
    // `pg_total_relation_size` is a catalog read, not a scan; table names come
    // from a fixed allowlist.
    let table_sizes: Vec<(String, i64)> = sqlx::query_as(
        "SELECT c.relname::text, pg_total_relation_size(c.oid)::int8
         FROM pg_class c
         JOIN pg_namespace n ON n.oid = c.relnamespace
         WHERE n.nspname = 'public' AND c.relname::text = ANY($1)",
    )
    .bind(&TRACKED_TABLES[..])
    .fetch_all(pool)
    .await?;
    let table_points: Vec<NumberDataPoint> = table_sizes
        .iter()
        .map(|(table, bytes)| point(*bytes as f64, &[("table", table.as_str())], nanos))
        .collect();

    let db_bytes: i64 = sqlx::query_scalar("SELECT pg_database_size(current_database())")
        .fetch_one(pool)
        .await?;
    // Oldest un-pruned raw metric: a rising value flags a stalled raw prune.
    let oldest_raw: Option<f64> =
        sqlx::query_scalar("SELECT extract(epoch FROM now() - min(time))::float8 FROM metrics")
            .fetch_one(pool)
            .await?;
    // Rollup lag: how far behind now() the newest maintained rollup bucket is.
    let rollup_lag: Option<f64> = sqlx::query_scalar(
        "SELECT extract(epoch FROM now() - max(bucket))::float8 FROM metric_series_rollups",
    )
    .fetch_one(pool)
    .await?;

    // Retention last-run: age from the last success, or from process start before
    // the first sweep — matching the /healthz staleness reference.
    let now_ts = Utc::now().timestamp();
    let last = RETENTION_LAST_SUCCESS_UNIX.load(Ordering::Relaxed);
    let reference = if last > 0 {
        last
    } else {
        process_started_unix()
    };
    let retention_age = (now_ts - reference) as f64;
    let retention_rows = RETENTION_LAST_SUCCESS_ROWS.load(Ordering::Relaxed) as f64;

    // Pool utilisation. sqlx 0.9 exposes total + idle (no waiter count).
    let pool_total = pool.size() as f64;
    let pool_idle = pool.num_idle() as f64;
    let pool_in_use = (pool_total - pool_idle).max(0.0);

    let mut metrics = vec![
        gauge("watcher.db.table_bytes", "By", table_points),
        gauge(
            "watcher.db.size_bytes",
            "By",
            vec![point(db_bytes as f64, &[], nanos)],
        ),
        gauge(
            "watcher.retention.last_success_age_seconds",
            "s",
            vec![point(retention_age, &[], nanos)],
        ),
        gauge(
            "watcher.retention.last_success_rows",
            "1",
            vec![point(retention_rows, &[], nanos)],
        ),
        gauge(
            "watcher.db.pool_connections",
            "1",
            vec![
                point(pool_in_use, &[("state", "in_use")], nanos),
                point(pool_idle, &[("state", "idle")], nanos),
                point(pool_total, &[("state", "total")], nanos),
            ],
        ),
        counter(
            "watcher.ingest.spans_total",
            "1",
            SPANS_INGESTED.load(Ordering::Relaxed),
            nanos,
        ),
        counter(
            "watcher.ingest.logs_total",
            "1",
            LOGS_INGESTED.load(Ordering::Relaxed),
            nanos,
        ),
        counter(
            "watcher.ingest.metric_points_total",
            "1",
            METRIC_POINTS_INGESTED.load(Ordering::Relaxed),
            nanos,
        ),
        sum(
            "watcher.ingest.dropped_total",
            "1",
            vec![
                point(
                    DROP_CAP.load(Ordering::Relaxed) as f64,
                    &[("reason", "cap")],
                    nanos,
                ),
                point(
                    DROP_DECODE.load(Ordering::Relaxed) as f64,
                    &[("reason", "decode")],
                    nanos,
                ),
                point(
                    DROP_INSERT.load(Ordering::Relaxed) as f64,
                    &[("reason", "insert")],
                    nanos,
                ),
            ],
        ),
    ];
    // Age gauges are absent (NULL) when the table is empty — skip rather than
    // reporting a misleading zero.
    if let Some(v) = oldest_raw {
        metrics.push(gauge(
            "watcher.retention.oldest_raw_metric_age_seconds",
            "s",
            vec![point(v, &[], nanos)],
        ));
    }
    if let Some(v) = rollup_lag {
        metrics.push(gauge(
            "watcher.rollup.lag_seconds",
            "s",
            vec![point(v, &[], nanos)],
        ));
    }

    // Index-only-scan / visibility-map health canary — see
    // `ROLLUP_TABLE` doc comment. `pg_stat_user_tables` is always available (no
    // extension needed); the row is absent only before the catalog's stats have
    // ever been populated for the table, which shouldn't happen post-migration
    // but is handled the same way as the other optional gauges above.
    let vacuum_stats: Option<(i64, i64, Option<f64>)> = sqlx::query_as(
        "SELECT n_live_tup, n_dead_tup, extract(epoch FROM now() - last_autovacuum)::float8
         FROM pg_stat_user_tables
         WHERE schemaname = 'public' AND relname = $1",
    )
    .bind(ROLLUP_TABLE)
    .fetch_optional(pool)
    .await?;
    if let Some((live, dead, autovacuum_age)) = vacuum_stats {
        if let Some(ratio) = dead_tuple_ratio(live, dead) {
            metrics.push(gauge(
                "watcher.db.dead_tuple_ratio",
                "1",
                vec![point(ratio, &[("table", ROLLUP_TABLE)], nanos)],
            ));
        }
        // NULL until the table's first autovacuum ever runs — skip rather than
        // reporting a misleading zero (that would read as "just vacuumed").
        if let Some(age) = autovacuum_age {
            metrics.push(gauge(
                "watcher.db.last_autovacuum_age_seconds",
                "s",
                vec![point(age, &[("table", ROLLUP_TABLE)], nanos)],
            ));
        }
    }

    // The all-visible fraction needs the `pg_visibility` contrib extension,
    // which isn't guaranteed to be installed (it's not a "trusted" extension,
    // so the app's non-superuser role can't `CREATE EXTENSION` it itself even if
    // it wanted to — installing it is a cluster-ops action, not read-only
    // introspection). Degrade gracefully: an undefined-function error just means
    // the extension isn't there, so skip the gauge rather than failing the whole
    // snapshot; the dead-tuple ratio and autovacuum age above still cover the
    // canary on their own.
    match rollup_vm_all_visible_fraction(pool).await {
        Ok(Some(fraction)) => metrics.push(gauge(
            "watcher.db.vm_all_visible_fraction",
            "1",
            vec![point(fraction, &[("table", ROLLUP_TABLE)], nanos)],
        )),
        Ok(None) => {}
        Err(e) if is_undefined_function(&e) => log_pg_visibility_missing_once(),
        Err(e) => tracing::warn!("self-telemetry: pg_visibility query failed: {e}"),
    }

    Ok(metrics)
}

/// `numerator / denominator`, or `None` when `denominator` is zero (an empty
/// or not-yet-analyzed table) rather than reporting a misleading 0.
fn safe_ratio(numerator: i64, denominator: i64) -> Option<f64> {
    (denominator > 0).then(|| numerator as f64 / denominator as f64)
}

/// Fraction of `ROLLUP_TABLE`'s tuples that are dead — the same quantity
/// autovacuum's own dead-tuple threshold check watches.
fn dead_tuple_ratio(n_live_tup: i64, n_dead_tup: i64) -> Option<f64> {
    safe_ratio(n_dead_tup, n_live_tup + n_dead_tup)
}

/// Fraction of `ROLLUP_TABLE`'s pages the visibility map marks all-visible —
/// index-only scans avoid a heap fetch only for these.
fn vm_all_visible_fraction(all_visible: i64, relpages: i64) -> Option<f64> {
    safe_ratio(all_visible, relpages)
}

/// `pg_visibility_map_summary` reads only the visibility-map fork (no heap
/// I/O), so this is cheap enough for the self-telemetry interval —
/// deliberately not `pg_visibility()`, which would read every heap page to
/// cross-check its flag against the VM bit. Errors with SQLSTATE `42883`
/// (undefined function) when the `pg_visibility` extension isn't installed;
/// the caller treats that as "unavailable", not a failure.
async fn rollup_vm_all_visible_fraction(pool: &PgPool) -> Result<Option<f64>, sqlx::Error> {
    let (all_visible, relpages): (i64, i64) = sqlx::query_as(
        "SELECT s.all_visible, c.relpages::int8
         FROM pg_visibility_map_summary($1::regclass) s, pg_class c
         WHERE c.oid = $1::regclass",
    )
    .bind(ROLLUP_TABLE)
    .fetch_one(pool)
    .await?;
    Ok(vm_all_visible_fraction(all_visible, relpages))
}

fn is_undefined_function(err: &sqlx::Error) -> bool {
    matches!(err, sqlx::Error::Database(db_err) if db_err.code().as_deref() == Some("42883"))
}

/// Logs the missing-extension notice once per process rather than every
/// self-telemetry tick.
fn log_pg_visibility_missing_once() {
    static LOGGED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if !LOGGED.swap(true, Ordering::Relaxed) {
        tracing::info!(
            "self-telemetry: pg_visibility extension not installed -- \
             watcher.db.vm_all_visible_fraction will not be emitted; \
             watcher.db.dead_tuple_ratio and watcher.db.last_autovacuum_age_seconds \
             still cover the index-only-scan health canary"
        );
    }
}

// ---------------------------------------------------------------------------
// OTLP construction helpers.
// ---------------------------------------------------------------------------

fn now_nanos() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

fn str_kv(key: &str, value: &str) -> KeyValue {
    KeyValue {
        key: key.to_string(),
        value: Some(AnyValue {
            value: Some(any_value::Value::StringValue(value.to_string())),
        }),
        ..Default::default()
    }
}

fn point(value: f64, attrs: &[(&str, &str)], nanos: u64) -> NumberDataPoint {
    NumberDataPoint {
        time_unix_nano: nanos,
        value: Some(number_data_point::Value::AsDouble(value)),
        attributes: attrs.iter().map(|(k, v)| str_kv(k, v)).collect(),
        ..Default::default()
    }
}

fn gauge(name: &str, unit: &str, data_points: Vec<NumberDataPoint>) -> Metric {
    Metric {
        name: name.to_string(),
        unit: unit.to_string(),
        data: Some(metric::Data::Gauge(Gauge { data_points })),
        ..Default::default()
    }
}

/// A cumulative monotonic sum — the metrics UI rates it into per-second throughput.
fn sum(name: &str, unit: &str, data_points: Vec<NumberDataPoint>) -> Metric {
    Metric {
        name: name.to_string(),
        unit: unit.to_string(),
        data: Some(metric::Data::Sum(Sum {
            data_points,
            is_monotonic: true,
            aggregation_temporality: 2, // cumulative
        })),
        ..Default::default()
    }
}

/// A single-point cumulative counter.
fn counter(name: &str, unit: &str, value: u64, nanos: u64) -> Metric {
    sum(name, unit, vec![point(value as f64, &[], nanos)])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retention_fresh_success_is_not_stalled() {
        // Swept 60s ago, 2h threshold → healthy.
        assert!(!retention_stalled(1_000_000, 999_000, 1_000_060, 7200));
    }

    #[test]
    fn retention_stale_success_is_stalled() {
        // Last success 3h ago, 2h threshold → stalled.
        assert!(retention_stalled(
            1_000_000,
            900_000,
            1_000_000 + 3 * 3600,
            7200
        ));
    }

    #[test]
    fn retention_never_ran_uses_process_start() {
        let now = 1_000_000;
        // Never succeeded (0). Just booted → not stalled yet.
        assert!(!retention_stalled(0, now - 60, now, 7200));
        // Never succeeded and up past the threshold → stalled (a loop that never
        // completes a sweep must not look healthy forever).
        assert!(retention_stalled(0, now - 3 * 3600, now, 7200));
    }

    #[test]
    fn health_ready_only_when_db_ok_and_not_stalled() {
        let ready = Health {
            db_ok: true,
            retention_stalled: false,
            retention_last_success_age_secs: Some(30),
        };
        assert!(ready.healthy());

        let db_down = Health {
            db_ok: false,
            retention_stalled: false,
            retention_last_success_age_secs: Some(30),
        };
        assert!(!db_down.healthy());

        let stalled = Health {
            db_ok: true,
            retention_stalled: true,
            retention_last_success_age_secs: Some(9999),
        };
        assert!(!stalled.healthy());
    }

    #[test]
    fn dead_tuple_ratio_divides_dead_by_total() {
        // 20 dead out of 100 total tuples → 20%.
        assert_eq!(dead_tuple_ratio(80, 20), Some(0.2));
    }

    #[test]
    fn dead_tuple_ratio_none_when_table_empty() {
        assert_eq!(dead_tuple_ratio(0, 0), None);
    }

    #[test]
    fn vm_all_visible_fraction_divides_by_relpages() {
        // 45 of 50 pages all-visible → 90%.
        assert_eq!(vm_all_visible_fraction(45, 50), Some(0.9));
    }

    #[test]
    fn vm_all_visible_fraction_none_when_no_pages() {
        assert_eq!(vm_all_visible_fraction(0, 0), None);
    }

    // `is_undefined_function` (the SQLSTATE-42883 check) isn't unit-tested with a
    // mock `DatabaseError` — matching `otlp::is_failover_error`, its peer
    // SQLSTATE-classifying helper, which is likewise only exercised against a
    // real Postgres error in an integration test rather than a hand-rolled
    // trait impl. See `rollup_vacuum_health_canary_*` in tests/smoke.rs.
}
