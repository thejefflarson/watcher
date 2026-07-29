# 0014. Self-monitoring: emit ops metrics in-process, deep /healthz gates readiness

- Status: Accepted
- Date: 2026-07-18
- Related: [0001](0001-postgres-only-no-clickhouse.md), [0010](0010-ui-embedded-in-server-binary.md), [0011](0011-metric-rollups.md), [0007](0007-retention-by-deletion.md)

## Context

watcher had no visibility into its own health. A silently stalled retention sweep let
the `metrics` table grow to tens of GB un-paged. We want watcher's own
operational signals (ingest throughput, drop counts, per-table on-disk bytes,
retention recency, rollup lag, pool utilisation) to be visible and alertable — using
the machinery watcher already has, on a Raspberry Pi, in a single binary.

Two shapes were possible for the ops metrics:
1. Export them over OTLP to `OTEL_EXPORTER_OTLP_ENDPOINT` (the same env the trace
   self-export uses), or
2. Hand them straight to the in-process ingest path (`otlp::store_metrics`).

The trace endpoint may point at an *external* collector; exporting there would not
guarantee the metrics land in watcher's *own* Postgres, which is the only place its
UI reads and its alert evaluator queries.

## Decision

- **Emit ops metrics in-process.** A `selfmon` background task snapshots the
  `watcher.*` gauges/counters on a timer and calls `store_metrics` directly — no
  network hop, no self-scrape loop, no metrics SDK/exporter wiring. Points are tagged
  `service.name=watcher`, so they ride the existing metrics UI and the normal,
  declarative alert-rule surface (ADR 0012). Self-emitted points count toward the
  ingest counter (they are real stored points); this is accepted and documented.
- **Retention last-run state is in-process** (atomics), not a new table — before the
  first sweep completes, staleness is measured from process start so a loop that never
  succeeds still trips the check. Restart resets the reference to boot time (accepted:
  no schema, Pi-light).
- **Deep `/healthz` gates readiness, not liveness.** It returns 200 only when the DB is
  reachable AND retention is not stalled past `WATCHER_HEALTHZ_MAX_RETENTION_AGE_SECS`
  (default 7200); otherwise 503. A stalled retention or DB outage should shed traffic
  and page, not kill the process.
- **New env knobs:** `WATCHER_SELF_TELEMETRY_INTERVAL_SECS` (default 60),
  `WATCHER_HEALTHZ_MAX_RETENTION_AGE_SECS` (default 7200). Self-telemetry shares the
  existing `WATCHER_SELF_TELEMETRY` off-switch.

## Consequences

- watcher's health rides its own UI and alerting with no extra infrastructure —
  consistent with Postgres-only (ADR 0001) and the single embedded binary (ADR 0010).
- The default alert rule for the new self-mon metric (e.g. retention-stall) is
  declarative and lives in the separate `../cluster` GitOps repo's chart values
  (`server.alerts`), reconciled on startup per ADR 0012 — not in this repo. Adding it
  is a required human follow-up.
- Readiness (not liveness) semantics must be wired correctly in the chart's probes; a
  liveness probe pointed at deep `/healthz` would crash-loop the pod on a DB blip.
