# 0011. Downsample metrics into rollup buckets

- Status: Accepted
- Date: 2026-05-30
- Refines: [0007](0007-retention-by-deletion.md)
- Refined by: [0020](0020-on-ingest-per-series-metric-rollups.md)

> **Partially superseded.** The downsample-into-buckets decision still stands, but
> the mechanism below is stale: there is no `rollup.rs` background sweep, no
> collapsed `metric_rollups` table (dropped in migration 0010), and no
> `WATCHER_METRICS_RAW_DAYS`. Rollups are now written per-series, on ingest, into
> `metric_series_rollups`, and raw-point retention is an hours window
> (`WATCHER_METRICS_RAW_HOURS`). See
> [0020](0020-on-ingest-per-series-metric-rollups.md).

## Context

[0007](0007-retention-by-deletion.md) bounded disk by deleting old rows and
explicitly deferred downsampling. For traces and logs that's fine — you rarely want
a week-old span. Metrics are different: the whole point is the trend line, and
deleting old points erases exactly the history a chart wants.

## Decision

A background task (`rollup.rs`) periodically folds raw `metrics` points into
fixed-width time buckets in a `metric_rollups` table (`count, sum, min, max, avg`
per `name`/`service`/`bucket`). Bucket width is `WATCHER_ROLLUP_BUCKET_SECS`
(default 300s; `0` disables). Retention then prunes **raw** points on a short
window (`WATCHER_METRICS_RAW_DAYS`, default 2) while keeping rollups for the full
`WATCHER_RETENTION_DAYS`. The series API stitches the two: rollups for older
buckets, raw points newer than the last rollup bucket, so there's no gap and no
double-count.

## Consequences

- Long-term metric history at bounded cost — a week of 5-minute buckets is tiny.
- Still no new infrastructure: one table, one upsert query, plain Postgres
  ([ADR 0001](0001-postgres-only-no-clickhouse.md)). Re-running a bucket is
  idempotent (upsert on `(name, service_key, bucket)`), so late points are absorbed.
- Old metrics are averaged, not exact — sub-bucket spikes are smoothed to the
  bucket's min/max/avg. Acceptable for trend charts; not for forensic replay.
- Traces and logs keep [0007](0007-retention-by-deletion.md)'s delete-only policy.
