//! watcher self-instrumentation of its own *traces* (JEF-462): a custom
//! [`opentelemetry_sdk::trace::SpanExporter`] that maps exported `SpanData` into the
//! in-process trace-ingest path ([`crate::otlp::store_traces`]) — the same table its
//! UI reads — tagged `service.name=watcher`.
//!
//! This is the trace analogue of [`crate::selfmon`]'s metrics (ADR 0014) and
//! [`crate::selflog`]'s logs (ADR 0016): all three self-signals are now in-process.
//! Traces were the last one still on the fragile OTLP self-export (a batch
//! `SpanExporter` POSTing to `localhost:4318`); that path had wedged into a
//! shut-down state and never landed a single watcher span while flooding the `logs`
//! table with "Spans are being emitted even after Shutdown" warnings (JEF-462).
//! Going in-process removes the network hop, the self-POST, and the batch-to-self
//! that could shut down — the exporter just enqueues converted spans for a drain
//! task that stores them on the main runtime.
//!
//! **Async boundary — a bounded channel, mirroring [`crate::selflog`].** The SDK's
//! batch processor drives [`SpanExporter::export`] on its *own* background thread,
//! but the sqlx `PgPool` and its connection reactors live on the main tokio runtime;
//! doing DB I/O from the batch thread would touch sockets registered with the wrong
//! reactor. So `export` only converts each `SpanData` into an OTLP `Span` and
//! `try_send`s it onto a bounded channel — cheap, non-blocking, thread-agnostic —
//! and a task spawned on the main runtime ([`drain`]) folds them into a batched
//! `ExportTraceServiceRequest` and calls `store_traces`. The channel is created
//! before the pool exists, so any span emitted during startup buffers rather than
//! panics; past capacity records are dropped and counted ([`DROPPED`]).
//!
//! **No feedback loop.** Storing a self-span runs sqlx queries that themselves emit
//! `tracing` events (and, under a verbose `RUST_LOG`, spans); capturing *those*
//! would recurse into more stored spans/logs. Two things close the loop:
//! * the store path is *not* `#[tracing::instrument]`ed and runs off the HTTP
//!   request path, so it creates no spans of its own (the same structural guard the
//!   old design relied on with "/api spanned, /v1 not"), and
//! * [`drain`] wraps every store in [`crate::selflog::store_guarded`] — the shared
//!   `STORING` task-local. While it is set, both `selflog`'s `on_event` and the otel
//!   layer's `dynamic_filter_fn` guard (installed in `main`) skip capture, so
//!   anything the store path emits produces no further stored span *or* log.

use std::sync::atomic::{AtomicU64, Ordering};

use opentelemetry_proto::tonic::{
    collector::trace::v1::ExportTraceServiceRequest,
    common::v1::{any_value, AnyValue, KeyValue},
    resource::v1::Resource,
    trace::v1::{ResourceSpans, ScopeSpans, Span},
};
use opentelemetry_sdk::error::OTelSdkResult;
use opentelemetry_sdk::trace::{SdkTracerProvider, SpanData, SpanExporter};
use sqlx::PgPool;
use tokio::sync::mpsc::{Receiver, Sender};

/// Channel capacity: the most converted spans that may buffer before the drain task
/// stores them. Bounds startup buffering (spans before `db::connect`) and any
/// transient store backlog; past it spans are dropped and counted ([`DROPPED`]).
const CHANNEL_CAPACITY: usize = 8192;

/// The most spans folded into a single `store_traces` request, so a burst is written
/// in a few batched round-trips rather than one per span.
const MAX_BATCH: usize = 512;

/// Count of self-spans dropped because the channel was full (the drain task fell
/// behind, or spans piled up before the pool was ready). Best-effort — surfaced only
/// for debugging; not wired into the metrics surface.
pub static DROPPED: AtomicU64 = AtomicU64::new(0);

/// The receiver end [`drain`] consumes. Aliased so `main` need not name the proto type.
pub type SpanReceiver = Receiver<Span>;

/// Self-trace capture shares the [`crate::selfmon::enabled`] switch
/// (`WATCHER_SELF_TELEMETRY`) — off ⇒ no metrics, logs, *or* traces.
pub fn enabled() -> bool {
    crate::selfmon::enabled()
}

/// Service name tag for stored self-spans — mirrors [`crate::selfmon`]'s and
/// [`crate::selflog`]'s, so all three self-signals share one `service.name` in the UI.
fn service_name() -> String {
    std::env::var("OTEL_SERVICE_NAME").unwrap_or_else(|_| "watcher".to_string())
}

/// A [`SpanExporter`] that converts exported spans into OTLP `Span`s and enqueues
/// them for the in-process store. Holds only the channel sender, so it is cheap and
/// safe to drive from the SDK's batch thread.
#[derive(Debug)]
pub struct LocalSpanExporter {
    tx: Sender<Span>,
}

/// Build the exporter and the receiver its drain task consumes. The channel is
/// created here (before the pool exists) so spans emitted during startup buffer
/// rather than panic; `main` spawns [`drain`] with the receiver once the pool is up.
pub fn channel_exporter() -> (LocalSpanExporter, SpanReceiver) {
    let (tx, rx) = tokio::sync::mpsc::channel(CHANNEL_CAPACITY);
    (LocalSpanExporter { tx }, rx)
}

/// Build the `SdkTracerProvider` around the in-process exporter: the batch processor
/// (its own thread) hands span batches to [`LocalSpanExporter::export`]. Kept as a
/// `SdkTracerProvider` + `tracing_opentelemetry` layer so all of watcher's span
/// timing / parent / status / attribute conversion is reused unchanged — only the
/// exporter's destination changes (the DB, not a network POST to self).
pub fn build_provider(exporter: LocalSpanExporter) -> SdkTracerProvider {
    SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .with_resource(
            opentelemetry_sdk::Resource::builder()
                .with_attribute(opentelemetry::KeyValue::new("service.name", service_name()))
                .build(),
        )
        .build()
}

impl SpanExporter for LocalSpanExporter {
    fn export(
        &self,
        batch: Vec<SpanData>,
    ) -> impl std::future::Future<Output = OTelSdkResult> + Send {
        // Sync + non-blocking: convert each SpanData to its OTLP form and enqueue it.
        // No DB I/O here — this runs on the batch processor's thread, off the main
        // runtime where the pool's connection reactors live. A full channel means the
        // drain fell behind (or the pool isn't up yet) — drop and count.
        for data in batch {
            let span: Span = data.into();
            if self.tx.try_send(span).is_err() {
                DROPPED.fetch_add(1, Ordering::Relaxed);
            }
        }
        std::future::ready(Ok(()))
    }
}

/// Drain forever: block for the next span, greedily batch any already-buffered ones,
/// and store the batch through the in-process ingest path under the reentrancy guard.
/// Returns only when the sender is dropped (never, in production).
pub async fn drain(pool: PgPool, mut rx: SpanReceiver) {
    while let Some(first) = rx.recv().await {
        let mut batch = vec![first];
        while batch.len() < MAX_BATCH {
            match rx.try_recv() {
                Ok(span) => batch.push(span),
                Err(_) => break,
            }
        }
        store_batch(&pool, batch).await;
    }
}

/// Drain everything currently buffered (without blocking for more) and store it.
/// Returns how many spans were stored. Exposed for tests.
pub async fn drain_pending(pool: &PgPool, rx: &mut SpanReceiver) -> usize {
    let mut batch = Vec::new();
    while let Ok(span) = rx.try_recv() {
        batch.push(span);
    }
    let n = batch.len();
    if n > 0 {
        store_batch(pool, batch).await;
    }
    n
}

/// Wrap a batch of spans in one OTLP request tagged `service.name=watcher` and store
/// it under the shared [`crate::selflog::store_guarded`] guard — the anti-feedback
/// core: while it is set, neither the otel layer nor the selflog layer captures the
/// events/spans the store path emits.
async fn store_batch(pool: &PgPool, spans: Vec<Span>) {
    let req = ExportTraceServiceRequest {
        resource_spans: vec![ResourceSpans {
            resource: Some(Resource {
                attributes: vec![str_kv("service.name", &service_name())],
                ..Default::default()
            }),
            scope_spans: vec![ScopeSpans {
                spans,
                ..Default::default()
            }],
            ..Default::default()
        }],
    };
    crate::selflog::store_guarded(async {
        crate::otlp::store_traces(pool, req).await;
    })
    .await;
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

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry::trace::{
        SpanContext, SpanId, SpanKind, Status, TraceFlags, TraceId, TraceState,
    };
    use opentelemetry::{InstrumentationScope, KeyValue as OtelKeyValue};
    use opentelemetry_sdk::trace::{SpanEvents, SpanLinks};
    use std::borrow::Cow;
    use std::time::{Duration, SystemTime};

    fn span_data(name: &'static str, trace: u128, span: u64) -> SpanData {
        SpanData {
            span_context: SpanContext::new(
                TraceId::from(trace),
                SpanId::from(span),
                TraceFlags::SAMPLED,
                false,
                TraceState::default(),
            ),
            parent_span_id: SpanId::INVALID,
            parent_span_is_remote: false,
            span_kind: SpanKind::Server,
            name: Cow::Borrowed(name),
            start_time: SystemTime::now(),
            end_time: SystemTime::now() + Duration::from_millis(5),
            attributes: vec![OtelKeyValue::new("http.route", "/api/traces")],
            dropped_attributes_count: 0,
            events: SpanEvents::default(),
            links: SpanLinks::default(),
            status: Status::Unset,
            instrumentation_scope: InstrumentationScope::builder("watcher-server").build(),
        }
    }

    #[tokio::test]
    async fn export_enqueues_converted_spans() {
        let (exporter, mut rx) = channel_exporter();
        exporter
            .export(vec![span_data("GET /api/traces", 0x1234, 0x56)])
            .await
            .expect("export ok");

        let span = rx.try_recv().expect("one span enqueued");
        // The SpanData → OTLP Span conversion preserves identity + name, so the stored
        // row keys and displays correctly.
        assert_eq!(span.name, "GET /api/traces");
        assert_eq!(span.trace_id, TraceId::from(0x1234u128).to_bytes().to_vec());
        assert_eq!(span.span_id, SpanId::from(0x56u64).to_bytes().to_vec());
        assert!(rx.try_recv().is_err(), "exactly one span");
    }

    #[tokio::test]
    async fn export_drops_and_counts_when_channel_full() {
        // A tiny channel so the second export overflows it deterministically.
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let exporter = LocalSpanExporter { tx };
        let before = DROPPED.load(Ordering::Relaxed);

        exporter
            .export(vec![span_data("a", 1, 1), span_data("b", 2, 2)])
            .await
            .expect("export ok");

        // First fits, second overflows and is counted rather than blocking.
        assert!(rx.try_recv().is_ok());
        assert!(rx.try_recv().is_err());
        assert_eq!(DROPPED.load(Ordering::Relaxed), before + 1);
    }
}
