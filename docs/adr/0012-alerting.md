# 0012. Threshold alerting with stored events and optional webhook

- Status: Accepted
- Date: 2026-05-30

## Context

A metrics backend that can't tell you when something is wrong is just a dashboard
you have to remember to look at. We want alerting without standing up a separate
rule engine (Prometheus + Alertmanager is more than a homelab wants), and it has to
fit the Postgres-only, single-binary shape ([ADR 0001](0001-postgres-only-no-clickhouse.md)).

## Decision

Alerting lives in the server. `alert_rules` stores threshold rules (`metric`,
optional `service`, `agg` ∈ avg|max|min|sum|last, `comparator` ∈ gt|lt,
`threshold`, `window_secs`). A background task (`alerts.rs`) evaluates enabled
rules every `WATCHER_ALERT_INTERVAL_SECS` (default 30): aggregate the window,
compare, and on a state change write an `alert_events` row — one open (unresolved)
event per rule, enforced by a partial unique index. Every transition is logged;
if `WATCHER_ALERT_WEBHOOK` is set, a JSON payload is POSTed on fire and resolve.
Rules are managed through `/api/alerts` (validated server-side) and surfaced in an
**Alerts** tab in the UI.

## Consequences

- Real alerting with two tables and a timer — no new service, no PromQL.
- `agg` is mapped to a whitelisted SQL aggregate, never interpolated, so a stored
  rule can't inject SQL even though the column is free text.
- Evaluation is poll-based on raw points, so alert latency is bounded by the
  interval and rules can only see within the raw-retention window
  ([ADR 0011](0011-metric-rollups.md)). No multi-condition or "for: 5m" sustained
  logic yet — a single breach of the windowed aggregate fires.
- Notification delivery is best-effort: a failed webhook is logged, not retried.
