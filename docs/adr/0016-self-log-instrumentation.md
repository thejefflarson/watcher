# 0016. Self-instrument watcher's own logs in-process

- Status: Accepted
- Date: 2026-07-19
- Related: [0014](0014-self-monitoring-in-process-metrics.md), [0004](0004-otlp-http-and-grpc.md), [0007](0007-retention-by-deletion.md), [0013](0013-auth-at-the-edge.md)

## Context

watcher self-instruments its own **traces** (OTLP `SpanExporter` + the
`tracing-opentelemetry` layer) and its own **metrics** (JEF-425 / ADR 0014:
`selfmon` hands ops gauges/counters straight to `otlp::store_metrics`, tagged
`service.name=watcher`). But its own **logs** only went to stdout via the `fmt`
layer — there was no `tracing`→logs bridge, so watcher's log lines never landed in
its own `logs` table. You could open a watcher self-trace but couldn't jump to the
correlated self-logs (the span→logs drill, JEF-429).

Two shapes were possible, mirroring the ADR 0014 metrics decision:

1. `opentelemetry-appender-tracing` + an OTLP `LogExporter` pointed back at watcher's
   own `/v1/logs`, or
2. a `tracing` Layer that converts each event into a log record and hands it to the
   in-process ingest path (`otlp::store_logs`) directly.

Exporting to self (1) needs a network hop and, worse, risks a **feedback loop**: the
log-export HTTP call and its handler emit their own `tracing` events, which the
appender captures and re-exports. Avoiding that needs an explicit anti-feedback
filter on the export path anyway.

## Decision

- **Capture self-logs in-process** (option 2), consistent with ADR 0014. A
  `selflog::SelfLogLayer` `tracing` Layer converts each event into an OTLP
  `LogRecord` and enqueues it; a spawned drain task folds records into a batched
  `ExportLogsServiceRequest` and calls `otlp::store_logs` — the same code the public
  `/v1/logs` ingest uses. No new schema (the `logs` table already exists), no network
  hop, no self-scrape. Points are tagged `service.name=watcher`, so self-logs ride the
  existing Logs view and share a `service.name` with self-metrics.

- **Async boundary via a bounded channel.** `Layer::on_event` is synchronous and must
  not block the emitting task or touch the DB, but the store is async. `on_event`
  converts the event to a record on the spot and `try_send`s it onto a bounded
  (`8192`) `tokio::mpsc`; the drain task `recv`s, greedily batches, and stores. The
  channel is created **before** `db::connect`, so events emitted during startup buffer
  rather than panic; `main` spawns the drain once the pool is ready.

- **Feedback loop is closed by a task-local reentrancy guard.** Storing a self-log
  runs sqlx queries and may log (`insert log failed: …`); capturing *those* would
  recurse. The drain task wraps every `store_logs` call in a `STORING` task-local
  scope, and `on_event` skips capture whenever that scope is active — so the layer
  never records the logs its own store path produces. (`sqlx=warn` in the default
  `EnvFilter` already drops sqlx's per-statement debug logs; the guard covers the
  store call site itself.) A dedicated unit test asserts capture is suppressed inside
  the guard and restored outside it, and an integration test asserts a drain of
  self-logs produces no further stored rows.

- **Level/target filtering rides the existing global `EnvFilter`.** The layer sits in
  the same `registry()` stack as the `fmt` and otel layers, under the one global
  `EnvFilter` (default `info,watcher_server=debug,sqlx=warn`, overridable via
  `RUST_LOG`). Self-logs therefore mirror exactly what `kubectl logs` shows — no
  separate level knob to drift. stdout logging is unchanged; this is purely additive.

- **Trace/span correlation.** `on_event` resolves the current span's OpenTelemetry
  context via `tracing_opentelemetry::get_otel_context(span_id, &dispatch)` and stores
  the trace/span ids on the record, so self-logs link to self-traces. Reading the live
  dispatch inside `on_event` is blocked by tracing's re-entrancy guard (it yields a
  no-op dispatch), so the layer captures a `WeakDispatch` in `on_register_dispatch` and
  upgrades it on demand. When the otel layer is absent (self-telemetry off) the ids are
  simply empty (stored NULL).

- **Opt-out.** Capture is gated by the shared `WATCHER_SELF_TELEMETRY` switch (off ⇒
  no traces, metrics, *or* logs) plus a log-specific `WATCHER_SELF_TELEMETRY_LOGS=0`
  that disables just log capture while leaving trace/metric self-telemetry on.

- **Bounded volume.** Self-logs share the `logs` retention (ADR 0007) and default to
  `info`. A full channel drops records and bumps a `DROPPED` counter rather than
  growing memory — self-logs are best-effort diagnostics, not the ingest surface.

## Consequences

- watcher's own logs are queryable and trace-correlated in its own UI with no extra
  infrastructure — consistent with Postgres-only (ADR 0001), the single embedded
  binary (ADR 0010), and the in-process metrics precedent (ADR 0014).
- Self-emitted logs count toward the `watcher.ingest.logs_total` counter (they are
  real stored rows), the same accepted trade-off ADR 0014 records for self-metrics.
- Startup buffering is capacity-bounded: a flood of events before the pool is ready
  (or a wedged drain) drops the overflow rather than accumulating unbounded — acceptable
  for best-effort diagnostics.
- `DROPPED` is not yet surfaced as a `watcher.*` metric; wiring it into `selfmon` is a
  possible follow-up if drop pressure ever needs alerting.
</content>
