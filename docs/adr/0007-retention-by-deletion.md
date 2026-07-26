# 0007. Retention by time-based deletion (defer downsampling)

- Status: Accepted
- Date: 2026-05-30

## Context

Telemetry grows without bound. A homelab has finite disk. Proper observability
backends downsample/roll up old data; that's a meaningful amount of machinery
(aggregation jobs, rollup tables, query-time stitching).

## Decision

For v0, a background task prunes rows older than `WATCHER_RETENTION_DAYS`
(default 7) hourly: `DELETE … WHERE time < now() - make_interval(days => $1)`.
`0` disables it. Downsampling/rollups are explicitly **out of scope** for now.

## Consequences

- Bounded disk with three lines of SQL and no new tables.
- Old data is gone, not summarized — you lose long-term trends. That's fine for a
  homelab default; if we want history, the follow-up is Timescale hypertables +
  continuous aggregates, which slots in under the same schema ([0001](0001-postgres-only-no-clickhouse.md)).
- Deletes are coarse (whole-row, by age); no per-service policy yet. Spans, logs,
  and metric rollups can each be given their own window via
  `WATCHER_RETENTION_{SPANS,LOGS,METRICS}_DAYS` (JEF-434, an omitted override
  falls back to `WATCHER_RETENTION_DAYS`) — per-service is still out of scope,
  since a per-service delete over these tables would need `ctid`-batching like
  the raw-metrics prune to avoid the statement-timeout failure mode.
