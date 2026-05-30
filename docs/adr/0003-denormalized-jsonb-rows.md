# 0003. One denormalized row per span/log/metric point, attributes as JSONB

- Status: Accepted
- Date: 2026-05-30

## Context

OTLP data is deeply nested (Resource → Scope → Span/LogRecord/DataPoint, each with
arbitrary key/value attributes). We could normalize (separate attribute tables,
resource/scope tables) or denormalize (flatten on ingest).

## Decision

We will **flatten on ingest** into three wide tables — `spans`, `logs`, `metrics` —
with one row per span / log record / metric data point. `service.name` is lifted
to a column; all other attributes are stored in a single `attributes JSONB` column.
Metric histograms store `count` + `sum` only.

## Consequences

- Writes are simple single-row inserts; reads are single-table scans with B-tree
  indexes on `trace_id`, `time`, `service`. No joins on the hot path (the service
  map is the one self-join).
- JSONB keeps ingest schema-flexible and lets us index/filter attributes later
  (GIN) without migrations.
- We denormalize resource attributes per row (some duplication) — cheap storage,
  simpler queries. Exponential histograms and summaries are dropped in v0.
