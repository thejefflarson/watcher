# 0001. Postgres is the only datastore (no ClickHouse)

- Status: Accepted
- Date: 2026-05-30

## Context

watcher exists because the popular self-hosted observability backends (SigNoz,
Uptrace, HyperDX, Coroot) are built on **ClickHouse**, which is painful to run on
ARM / Raspberry Pi — to the point of maintaining a hand-built `clickhouse-pi`
image. A survey of alternatives found object-storage options (OpenObserve,
GreptimeDB) but no maintained Postgres-native, all-three-pillars, built-in-UI
backend (Promscale was discontinued in 2023). The target is a small k3s homelab
on 8–16 GB ARM nodes that already runs Postgres.

## Decision

We will store all telemetry — traces, logs, and metrics — in **PostgreSQL only**.
No ClickHouse, no object store, no separate metadata DB. Timescale is optional and
additive (hypertables), never required.

## Consequences

- Operationally trivial on the homelab: one familiar database, backups, and the
  existing postgres-operator. Runs comfortably on a Pi.
- We accept that Postgres won't match a columnar store at very high cardinality /
  volume. That's an acceptable trade for a homelab; revisit with Timescale
  hypertables + rollups if it ever bites (see [0007](0007-retention-by-deletion.md)).
- Query patterns must be Postgres-shaped (indexes, JSONB), not columnar.
