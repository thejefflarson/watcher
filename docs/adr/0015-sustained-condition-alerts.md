# 0015. Sustained-condition alerts (`for: 5m`)

- Status: Accepted
- Date: 2026-07-18
- Related: [0012](0012-alerting.md), [0007](0007-retention-by-deletion.md)

## Context

An alert rule fired on a single windowed-aggregate breach: one evaluation tick over
threshold opened a firing event and paged. A one-off spike — a brief GC pause, a
deploy blip, a Postgres HA failover — therefore paged immediately and then resolved
on the next tick. That flapping is exactly what pushed the operator to bolt on an
out-of-band watchdog. Prometheus-style rules solve this with `for: 5m`: the condition
must hold *continuously* for a dwell window before it fires.

We want the same, within the constraints watcher already lives under: declarative
config reconciled into `alert_rules` on startup, a single in-server evaluation loop,
no new machinery (ADR 0012). The reconcile step upserts the declared rule set by
name, so `alert_rules` rows are effectively read-only runtime state — mutable
per-tick state must not live there or every reconcile would stomp it.

## Decision

- **Add optional `for_secs` to `RuleConfig` / `alert_rules`.** NULL or 0 keeps the
  exact single-breach behavior; a positive value is the dwell window. It is validated
  to `1..=10800s` (3h) — well under the 6h raw-metric retention floor (ADR 0007), so a
  full dwell window of points is still queryable when the rule matures.
- **Pending state lives in `alert_events`, never on `alert_rules`.** A new
  `active_at TIMESTAMPTZ` column records when an event actually fired. On the first
  breach the evaluator opens an event with `active_at NULL` (a *pending* event whose
  `fired_at` is the breaching-since instant). Once the event has been open — and the
  condition is still breaching — for at least `for_secs` (`now() - fired_at >=
  for_secs`), the evaluator sets `active_at = now()` and *only then* notifies. This
  reuses the existing partial unique index (one open event per rule), which carries
  the pending row unchanged.
- **A flap that clears before maturing pages no one.** If the condition recovers
  while the open event is still pending (`active_at IS NULL`), the event is deleted
  silently — nothing fired, so there is nothing to resolve. A recovery *after*
  activation resolves and notifies exactly as before.
- **`firing` means activated.** The read surface (`/api/alerts`, `/api/alerts/events`)
  treats an open event as firing only when `active_at IS NOT NULL`, so a still-dwelling
  breach never surfaces as firing and a pending event is not shown as a transition.
- **Restart preserves pending state.** Because the pending event is a durable
  `alert_events` row carrying `fired_at`, a restart mid-dwell does *not* reset the
  clock — the breach's elapsed time survives, and the rule matures on schedule after
  the process comes back. This is the more correct behavior (a restart is not a
  recovery) and costs nothing extra, since the state is already persisted.

## Consequences

- Sustained-condition alerts are expressible declaratively with one new config key;
  rules without `for_secs` are byte-for-byte unchanged in behavior.
- Resolve semantics, the one-open-event-per-rule invariant, and the notification
  sinks are untouched — activation simply gates when the *firing* notification is sent.
- A rule with a `for_secs` longer than the raw retention window could never observe a
  full dwell of points; validation rejects that at reconcile so a misconfiguration
  fails loudly on startup rather than silently never firing.
- The evaluator now issues one extra small indexed lookup per rule per tick (the open
  event's activation/maturity state). At watcher's rule counts and tick cadence this
  is negligible.
