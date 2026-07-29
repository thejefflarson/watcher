# 0017. Self-instrument watcher's own traces in-process

- Status: Accepted
- Date: 2026-07-21
- Related: [0014](0014-self-monitoring-in-process-metrics.md), [0016](0016-self-log-instrumentation.md), [0004](0004-otlp-http-and-grpc.md), [0007](0007-retention-by-deletion.md), [0013](0013-auth-at-the-edge.md)

## Context

watcher self-instruments its own **metrics** (ADR 0014) and **logs** (ADR 0016)
*in-process* — handing them straight to `otlp::store_metrics` / `store_logs`, tagged
`service.name=watcher`. Its own **traces** were the last self-signal still on the
original network path: an `opentelemetry-otlp` batch `SpanExporter` POSTing OTLP over
HTTP back to watcher's own `:4318/v1/traces`.

That path was **dead**. The batch processor had wedged into a shut-down
state and logged "Spans are being emitted even after Shutdown ... Spans will not be
exported" on every span. Evidence from prod: **0** watcher spans in the `spans` table
ever, while self-metrics and self-logs (in-process) worked; and ~74.8k of the ~75.1k
self-log rows were *that one warning* — the broken exporter flooding the `logs` table
via the self-log capture. Consequences: watcher never appeared in the Services
pulldown (`/api/services` reads `spans`), no self-traces to correlate, and ~75k junk
rows.

The same two shapes ADR 0014/0016 weighed applied again:

1. keep exporting OTLP to self (the status quo — network hop, and a genuine feedback
   risk since the export POST and its handler emit their own spans), or
2. hand spans to the in-process ingest path directly.

## Decision

- **Capture self-traces in-process** (option 2), completing the lineage: **all three**
  self-signals — metrics (0014), logs (0016), traces (this ADR) — now go straight to
  Postgres with no network hop and no self-POST that can wedge shut.

- **A custom `SpanExporter`, not a bespoke tracing Layer.** `selftrace::LocalSpanExporter`
  implements `opentelemetry_sdk::trace::SpanExporter`; its `export` converts each
  `SpanData` into an OTLP `Span` (via `opentelemetry-proto`'s existing
  `From<SpanData>`) and enqueues it. The `SdkTracerProvider` + `tracing_opentelemetry`
  layer are **kept** — only `.with_batch_exporter(otlp_http)` is swapped for the local
  exporter. This reuses all of watcher's span timing / parent / status / attribute
  conversion unchanged, rather than reinventing it in a span-capturing Layer (the
  documented fallback, not needed here). The `opentelemetry-otlp` dependency is dropped.

- **Async boundary via a bounded channel** (as ADR 0016). The SDK's batch processor
  drives `export` on its *own* background thread, but the sqlx `PgPool`'s connection
  reactors live on the main tokio runtime — doing DB I/O from the batch thread would
  poll sockets on the wrong reactor. So `export` is sync + non-blocking: it converts
  and `try_send`s onto a bounded (`8192`) channel, and a drain task **spawned on the
  main runtime** folds spans into a batched `ExportTraceServiceRequest` and calls
  `store_traces`. The channel exists before `db::connect`, so startup spans buffer
  rather than drop; past capacity spans are dropped and counted (`DROPPED`).

- **Feedback loop closed two ways.** Storing a self-span runs sqlx queries that emit
  their own `tracing` events (and, under a verbose `RUST_LOG`, spans); capturing those
  would recurse. (a) *Structural:* the store path (`drain` → `store_traces`) is not
  `#[instrument]`ed and runs off the HTTP request path, so it creates no spans of its
  own — the same guarantee the old design leaned on with "/api spanned, /v1 not".
  (b) *Shared guard:* the drain wraps every store in `selflog::store_guarded`, the same
  `STORING` task-local selflog uses. While it is set, `selflog`'s `on_event` skips, and
  the otel layer now carries a `dynamic_filter_fn(|_,_| !selflog::suppressed())` per-layer
  filter that skips too — so anything the store path emits produces no further stored
  span *or* log. `dynamic_filter_fn` (not `filter_fn`) is required: the latter caches
  the callsite decision, which would defeat a task-local check.

- **Opt-out & retention.** Self-traces share the `WATCHER_SELF_TELEMETRY` off-switch
  (off ⇒ no metrics, logs, *or* traces) and the existing `spans` retention (ADR 0007);
  no new schema, no per-signal knob.

## Consequences

- watcher appears in its own Services view and its self-traces are queryable and
  correlate with self-logs/metrics — consistent with Postgres-only (ADR 0001), the
  single embedded binary (ADR 0010), and the 0014/0016 in-process precedent.
- Self-emitted spans count toward `watcher.ingest.spans_total` (they are real stored
  rows) — the same accepted trade-off ADR 0014/0016 record for self-metrics/logs.
- The ~74.8k historical "emitted even after Shutdown" rows in `logs`
  (`service='watcher'`) are inert once this ships (the broken exporter is gone). They
  can be one-time-deleted post-deploy
  (`DELETE FROM logs WHERE service='watcher' AND body LIKE '%emitted even after Shutdown%'`)
  or simply left to age out under normal `logs` retention — deletion is optional
  cleanup, not required for correctness.
- `DROPPED` (dropped self-spans) is not surfaced as a `watcher.*` metric — same
  best-effort posture, and same possible follow-up, as selflog's counter.
