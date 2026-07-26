# 0020. On-ingest per-series metric rollups

- Status: Accepted
- Date: 2026-07-26
- Refines: [0011](0011-metric-rollups.md)

## Context

[0011](0011-metric-rollups.md) described a background `rollup.rs` sweep that
periodically folded raw `metrics` points into a collapsed `metric_rollups` table
(one row per `name`/`service`/`bucket`, attributes discarded) on a timer, while
retention pruned raw points on a short day-count window
(`WATCHER_METRICS_RAW_DAYS`). That design shipped and was then replaced before it
saw much use: collapsing away attributes meant per-series breakdowns (the faceted
metric views, the expandable per-series list) had nowhere cheap to read from — they
had to fall back to scanning raw `metrics` rows — and a periodic sweep meant
rollups always lagged the sweep interval.

## Decision

Rollups are now written **on ingest**, per series, in the same statement that
writes the raw point — there is no background sweep and no `rollup.rs`.

- `otlp.rs`'s `flush_numbers`/`flush_histograms` batch every numeric/histogram
  point from one OTLP request and pass it to `insert_numbers`/`insert_histograms`,
  each a single aggregating SQL statement. A CTE writes the batch's raw rows into
  `metrics`, then a second `INSERT ... SELECT ... GROUP BY` collapses points that
  share `(name, series_key, bucket)` and upserts them into `metric_series_rollups`
  via `ON CONFLICT (name, series_key, bucket) DO UPDATE`, accumulating
  `count`/`sum`/`min`/`max`/`avg` (and, for histograms, `bucket_bounds` /
  `bucket_counts`, summed element-wise by the `array_sum` aggregate from
  `array_add`). The whole batch write goes through `write_with_failover_retry`
  (JEF-496), so a Patroni failover retries the statement on a fresh connection.
- **Series identity is preserved, not collapsed.** `series_key` is
  `metric_series_key(service, attrs)` — `md5(coalesce(service,'') || '|' ||
  attrs::text)` (`0009_metric_sql_helpers.sql`) — a stable hash of the service plus
  the full per-point attribute set. The `attrs` JSONB itself is stored alongside
  `series_key` on every rollup row, so per-series values and breakdowns are read
  directly out of `metric_series_rollups` without touching raw points.
  `metric_series_rollups` (`0007_metric_series_rollups.sql`) is keyed
  `PRIMARY KEY (name, series_key, bucket)`.
- **Bucketing** uses the shared `metric_bucket(t, width)` SQL function
  (`0009_metric_sql_helpers.sql`) to floor a timestamp to its bucket, with `width`
  seconds from `WATCHER_ROLLUP_BUCKET_SECS` (default 300). Both the ingest path
  (`otlp.rs`'s `rollup_width()`) and the read path (`api.rs`'s
  `rollup_bucket_secs()`) read the same env var so ingest-written buckets line up
  with what queries expect.
- Each upsert's rows are locked in a fixed `ORDER BY name, series_key, bucket`
  before the `ON CONFLICT` so concurrent ingest batches can't deadlock acquiring
  the same hot current-bucket row in different orders.
- The old collapsed `metric_rollups` table was dropped in
  `0010_drop_metric_rollups.sql` once nothing wrote it anymore.

Retention now has two independent windows (`retention.rs`):

- `metric_series_rollups` ages out on the same long window as spans/logs
  (`WATCHER_RETENTION_DAYS`, default 7) in `prune_once`'s per-table sweep.
- Raw `metrics` points are pruned on a much shorter window,
  `WATCHER_METRICS_RAW_HOURS` (default 6; `<= 0` disables raw pruning), via
  `prune_raw_metrics`, which deletes in `ctid`-bounded batches of 50,000 rows so a
  large backlog drains across several statements instead of one that could exceed
  `statement_timeout`.

Because rollups are written synchronously with the raw point rather than by a
lagging periodic sweep, there is no gap to stitch: the read path (`api.rs`'s
`metric_series` and friends) reads only from `metric_series_rollups`, even for the
current, still-filling bucket.

## Consequences

- No background sweep to run, tune, or fall behind on — a rollup row reflects
  every point ingested for it, including the current bucket, and re-ingesting an
  overlapping batch (e.g. after a retry) is safe because the upsert just
  accumulates.
- Per-series attributes survive past the raw-point retention window, which is
  what makes the faceted/per-series views cheap: they read
  `metric_series_rollups` directly instead of scanning raw `metrics`.
- Raw points only need an hours-wide window, not days, since long-term history
  lives in the rollups — this is what actually bounds the size of `metrics`, the
  highest-volume table (see `retention.rs`'s batched delete, added after it once
  grew to tens of GB unpruned).
- The extra aggregation work (`GROUP BY` + upsert) now happens inline on every
  ingest batch rather than off-peak; batching (JEF-495) and the fixed lock order
  keep it from becoming an ingest bottleneck, but it does mean ingest latency and
  rollup-write cost are coupled.
- Still the same trend-off as [0011](0011-metric-rollups.md): history beyond the
  raw window is bucket-averaged, not exact — fine for charts, not forensic replay.
