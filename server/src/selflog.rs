//! watcher self-instrumentation of its *own logs* (JEF-452): a `tracing` Layer
//! that converts each event into an OTLP log record and hands it to the in-process
//! ingest path ([`crate::otlp::store_logs`]) — the same table its UI reads — tagged
//! `service.name=watcher`.
//!
//! This mirrors [`crate::selfmon`]'s in-process metrics (ADR 0014, ADR 0016): no
//! network hop, no self-scrape loop, and — critically — no feedback loop. Storing a
//! self-log runs sqlx queries which themselves emit `tracing` events; capturing
//! *those* would recurse. A task-local guard ([`STORING`]) suppresses capture for
//! the duration of the store call, so the layer never records the logs produced by
//! its own store path. (`sqlx=warn` in the default EnvFilter already drops sqlx's
//! per-statement debug logs; the guard covers the store call site itself.)
//!
//! `on_event` is synchronous but the store is async, so events are converted to
//! records on the spot and pushed onto a bounded channel that a spawned task drains
//! into `store_logs`. Events emitted before the pool is ready (startup) buffer in
//! the channel up to its capacity; once full, further records are dropped and
//! counted rather than growing memory unbounded — self-logs are best-effort
//! diagnostics, not the ingest surface itself.
//!
//! Level and target filtering ride the *same* global `EnvFilter` as stdout, so
//! self-logs mirror exactly what `kubectl logs` shows (default `info`, with
//! `watcher_server=debug`). Trace/span ids are read from the current span's
//! `tracing-opentelemetry` context so self-logs correlate with self-traces.

use std::sync::atomic::{AtomicU64, Ordering};

use opentelemetry::trace::TraceContextExt;
use opentelemetry_proto::tonic::{
    collector::logs::v1::ExportLogsServiceRequest,
    common::v1::{any_value, AnyValue, KeyValue},
    logs::v1::{LogRecord, ResourceLogs, ScopeLogs},
    resource::v1::Resource,
};
use sqlx::PgPool;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc::{Receiver, Sender};
use tracing::dispatcher::{Dispatch, WeakDispatch};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::Layer;

/// Channel capacity: the most self-log records that may buffer before the drain
/// task stores them. Bounds startup buffering (events before `db::connect`) and any
/// transient store backlog; past it records are dropped and counted (`DROPPED`).
const CHANNEL_CAPACITY: usize = 8192;

/// The most records folded into a single `store_logs` request, so a burst is
/// written in a few batched round-trips rather than one per record.
const MAX_BATCH: usize = 512;

/// Count of self-log records dropped because the channel was full (the drain task
/// fell behind, or events piled up before the pool was ready). Best-effort — surfaced
/// only for debugging; not wired into the metrics surface.
pub static DROPPED: AtomicU64 = AtomicU64::new(0);

tokio::task_local! {
    /// Set for the duration of a `store_logs` call. While set, [`on_event`
    /// capture][SelfLogLayer::on_event] is suppressed on this task, so the events
    /// the store path emits (sqlx, insert-failure warnings) do not recurse into
    /// another stored log.
    static STORING: ();
}

/// True when the current task is inside a guarded store call — capture must be
/// skipped. Exposed for tests.
pub fn suppressed() -> bool {
    STORING.try_with(|_| ()).is_ok()
}

/// Run `fut` under the reentrancy guard, so any `tracing` event it emits is not
/// captured-and-stored. Exposed for tests.
pub async fn store_guarded<F: std::future::Future>(fut: F) -> F::Output {
    STORING.scope((), fut).await
}

/// Self-log capture is on unless self-telemetry is off ([`crate::selfmon::enabled`])
/// or `WATCHER_SELF_TELEMETRY_LOGS` is `0`/`false`/`off` — the latter turns off *just*
/// log capture while leaving trace/metric self-telemetry on.
pub fn enabled() -> bool {
    crate::selfmon::enabled()
        && !std::env::var("WATCHER_SELF_TELEMETRY_LOGS")
            .map(|v| matches!(v.as_str(), "0" | "false" | "off"))
            .unwrap_or(false)
}

/// Service name tag for stored self-logs — mirrors [`crate::selfmon`]'s, so self-logs
/// and self-metrics share one `service.name` in the UI.
fn service_name() -> String {
    std::env::var("OTEL_SERVICE_NAME").unwrap_or_else(|_| "watcher".to_string())
}

/// A `tracing` Layer that converts events into OTLP log records and enqueues them for
/// the in-process store. Holds the channel sender plus a weak handle to its own
/// `Dispatch`, captured in [`on_register_dispatch`][SelfLogLayer::on_register_dispatch]
/// so `on_event` can resolve the current span's OpenTelemetry context (reading the
/// live dispatch during event dispatch is blocked by tracing's re-entrancy guard).
pub struct SelfLogLayer {
    tx: Sender<LogRecord>,
    dispatch: OnceLock<WeakDispatch>,
}

/// Build the layer and the receiver its drain task consumes. The channel is created
/// here (before the pool exists) so events emitted during startup buffer rather than
/// panic; `main` spawns [`drain`] with the receiver once the pool is ready.
pub fn channel_layer() -> (SelfLogLayer, Receiver<LogRecord>) {
    let (tx, rx) = tokio::sync::mpsc::channel(CHANNEL_CAPACITY);
    (
        SelfLogLayer {
            tx,
            dispatch: OnceLock::new(),
        },
        rx,
    )
}

impl<S> Layer<S> for SelfLogLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_register_dispatch(&self, subscriber: &Dispatch) {
        // Stash a weak handle to the subscriber's dispatch — the only way to reach the
        // otel layer's span context from inside `on_event` (a live `get_default` there
        // returns a no-op dispatch).
        let _ = self.dispatch.set(subscriber.downgrade());
    }

    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        // Never capture the logs produced by our own store path.
        if suppressed() {
            return;
        }
        let (trace_id, span_id) = self.span_ids(&ctx);
        let record = build_record(event, trace_id, span_id);
        // Non-blocking: on_event is sync and must not block the emitting task. A full
        // channel means the drain fell behind (or the pool isn't up yet) — drop and
        // count rather than grow memory.
        if self.tx.try_send(record).is_err() {
            DROPPED.fetch_add(1, Ordering::Relaxed);
        }
    }
}

impl SelfLogLayer {
    /// Resolve the current span's `(trace_id, span_id)` as raw OTLP-shaped bytes.
    /// Empty when not inside a span, or when the otel layer is absent (self-telemetry
    /// off) — `store_logs` stores empty ids as NULL. The trace id is shared by the
    /// whole span tree; the span id is this span's own.
    fn span_ids<S>(&self, ctx: &Context<'_, S>) -> (Vec<u8>, Vec<u8>)
    where
        S: Subscriber + for<'a> LookupSpan<'a>,
    {
        let empty = (Vec::new(), Vec::new());
        let Some(current) = ctx.lookup_current() else {
            return empty;
        };
        let Some(dispatch) = self.dispatch.get().and_then(WeakDispatch::upgrade) else {
            return empty;
        };
        let Some(cx) = tracing_opentelemetry::get_otel_context(&current.id(), &dispatch) else {
            return empty;
        };
        let sc = cx.span().span_context().clone();
        if sc.is_valid() {
            (
                sc.trace_id().to_bytes().to_vec(),
                sc.span_id().to_bytes().to_vec(),
            )
        } else {
            empty
        }
    }
}

/// Convert one event into an OTLP `LogRecord`, attaching the current span's
/// trace/span ids and the event's fields as attributes.
fn build_record(event: &Event<'_>, trace_id: Vec<u8>, span_id: Vec<u8>) -> LogRecord {
    let meta = event.metadata();
    let (severity_number, severity_text) = severity(meta.level());

    let mut visitor = Visitor::default();
    event.record(&mut visitor);
    // The event's target (module path) is useful for filtering self-logs in the UI.
    visitor.attrs.push(str_kv("target", meta.target()));

    let nanos = now_nanos();
    LogRecord {
        time_unix_nano: nanos,
        observed_time_unix_nano: nanos,
        severity_number,
        severity_text: severity_text.to_string(),
        body: visitor.body.map(|b| AnyValue {
            value: Some(any_value::Value::StringValue(b)),
        }),
        attributes: visitor.attrs,
        trace_id,
        span_id,
        ..Default::default()
    }
}

/// Map a `tracing` level to an OTLP severity number + text (uppercase, per the OTLP
/// convention). Numbers follow the OTLP severity ranges: TRACE=1, DEBUG=5, INFO=9,
/// WARN=13, ERROR=17.
fn severity(level: &Level) -> (i32, &'static str) {
    match *level {
        Level::ERROR => (17, "ERROR"),
        Level::WARN => (13, "WARN"),
        Level::INFO => (9, "INFO"),
        Level::DEBUG => (5, "DEBUG"),
        Level::TRACE => (1, "TRACE"),
    }
}

/// Field visitor: pulls the event's `message` into the log body and every other field
/// into a typed OTLP attribute.
#[derive(Default)]
struct Visitor {
    body: Option<String>,
    attrs: Vec<KeyValue>,
}

impl Visitor {
    fn push(&mut self, key: &str, value: any_value::Value) {
        self.attrs.push(KeyValue {
            key: key.to_string(),
            value: Some(AnyValue { value: Some(value) }),
            ..Default::default()
        });
    }
}

impl tracing::field::Visit for Visitor {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "message" {
            self.body = Some(value.to_string());
        } else {
            self.push(
                field.name(),
                any_value::Value::StringValue(value.to_string()),
            );
        }
    }
    fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
        self.push(field.name(), any_value::Value::IntValue(value));
    }
    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        self.push(field.name(), any_value::Value::IntValue(value as i64));
    }
    fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
        self.push(field.name(), any_value::Value::BoolValue(value));
    }
    fn record_f64(&mut self, field: &tracing::field::Field, value: f64) {
        self.push(field.name(), any_value::Value::DoubleValue(value));
    }
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        let s = format!("{value:?}");
        if field.name() == "message" {
            self.body = Some(s);
        } else {
            self.push(field.name(), any_value::Value::StringValue(s));
        }
    }
}

fn str_kv(key: &str, value: &str) -> KeyValue {
    KeyValue {
        key: key.to_string(),
        value: Some(AnyValue {
            value: Some(any_value::Value::StringValue(value.to_string())),
        }),
        ..Default::default()
    }
}

fn now_nanos() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

/// Drain forever: block for the next record, greedily batch any already-buffered ones,
/// and store the batch through the in-process ingest path under the reentrancy guard.
/// Returns only when the sender is dropped (never, in production).
pub async fn drain(pool: PgPool, mut rx: Receiver<LogRecord>) {
    while let Some(first) = rx.recv().await {
        // Grows only during a burst; a typical low-volume self-log batch is 1–2.
        let mut batch = vec![first];
        while batch.len() < MAX_BATCH {
            match rx.try_recv() {
                Ok(rec) => batch.push(rec),
                Err(_) => break,
            }
        }
        store_batch(&pool, batch).await;
    }
}

/// Drain everything currently buffered (without blocking for more) and store it.
/// Returns how many records were stored. Exposed for tests.
pub async fn drain_pending(pool: &PgPool, rx: &mut Receiver<LogRecord>) -> usize {
    let mut batch = Vec::new();
    while let Ok(rec) = rx.try_recv() {
        batch.push(rec);
    }
    let n = batch.len();
    if n > 0 {
        store_batch(pool, batch).await;
    }
    n
}

/// Wrap a batch of records in one OTLP request and store it under the guard.
async fn store_batch(pool: &PgPool, records: Vec<LogRecord>) {
    let req = ExportLogsServiceRequest {
        resource_logs: vec![ResourceLogs {
            resource: Some(Resource {
                attributes: vec![str_kv("service.name", &service_name())],
                ..Default::default()
            }),
            scope_logs: vec![ScopeLogs {
                log_records: records,
                ..Default::default()
            }],
            ..Default::default()
        }],
    };
    // The guard makes every `tracing` event emitted by the store path (sqlx,
    // insert-failure warnings) invisible to this layer — the anti-feedback core.
    store_guarded(async {
        crate::otlp::store_logs(pool, req).await;
    })
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn severity_maps_each_level() {
        assert_eq!(severity(&Level::ERROR), (17, "ERROR"));
        assert_eq!(severity(&Level::WARN), (13, "WARN"));
        assert_eq!(severity(&Level::INFO), (9, "INFO"));
        assert_eq!(severity(&Level::DEBUG), (5, "DEBUG"));
        assert_eq!(severity(&Level::TRACE), (1, "TRACE"));
    }

    #[tokio::test]
    async fn store_guard_suppresses_capture_only_inside_scope() {
        // The anti-feedback invariant: capture is on by default, off while a store
        // runs, and on again afterward — so the events store_logs itself emits are
        // never captured and re-stored.
        assert!(!suppressed(), "capture on outside a store");
        store_guarded(async {
            assert!(suppressed(), "capture suppressed inside the store guard");
        })
        .await;
        assert!(!suppressed(), "capture restored after the store");
    }
}
