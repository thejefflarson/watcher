//! OTLP decode + storage. `store_*` are transport-agnostic (HTTP handlers and the
//! gRPC services both call them); the `ingest_*` fns are the HTTP/protobuf entrypoints.

use axum::{
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
};
use chrono::{DateTime, Utc};
use opentelemetry_proto::tonic::{
    collector::logs::v1::ExportLogsServiceRequest,
    collector::metrics::v1::ExportMetricsServiceRequest,
    collector::trace::v1::ExportTraceServiceRequest,
    common::v1::{any_value, AnyValue, KeyValue},
    logs::v1::LogRecord,
    metrics::v1::{metric, number_data_point, Exemplar, Metric, NumberDataPoint},
    trace::v1::Span,
};
use prost::Message;
use serde_json::json;
use sqlx::{PgConnection, PgPool};
use std::io::Read;
use std::sync::atomic::Ordering;
use std::time::Duration;

// ---------------------------------------------------------------------------
// HTTP entrypoints
// ---------------------------------------------------------------------------

/// Cap on a decompressed request body. Axum's default 2 MB limit bounds the
/// *compressed* body, but gzip can expand ~1000:1, so without a ceiling a tiny
/// request could balloon to gigabytes and OOM the server (a decompression bomb).
/// 64 MiB is far above any legitimate OTLP batch yet stops the bomb.
const MAX_DECOMPRESSED_BYTES: u64 = 64 * 1024 * 1024;

/// Decompress the body if it's gzip-encoded. Most OTLP exporters (the OTel
/// Collector, Traefik, the SDKs) gzip by default, so without this they 400.
fn payload(headers: &HeaderMap, body: Bytes) -> Result<Vec<u8>, std::io::Error> {
    let gzipped = headers
        .get(axum::http::header::CONTENT_ENCODING)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.contains("gzip"))
        .unwrap_or(false)
        || body.starts_with(&[0x1f, 0x8b]); // gzip magic, as a fallback
    if gzipped {
        let mut out = Vec::new();
        // Read through a limited reader: take(MAX + 1) so that crossing the cap is
        // detectable, then reject — bounding memory before we allocate gigabytes.
        flate2::read::GzDecoder::new(&body[..])
            .take(MAX_DECOMPRESSED_BYTES + 1)
            .read_to_end(&mut out)?;
        if out.len() as u64 > MAX_DECOMPRESSED_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "decompressed body exceeds limit",
            ));
        }
        Ok(out)
    } else {
        Ok(body.to_vec())
    }
}

pub async fn ingest_traces(
    State(pool): State<PgPool>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    let bytes = match payload(&headers, body) {
        Ok(b) => b,
        Err(e) => return (StatusCode::BAD_REQUEST, format!("gzip error: {e}")),
    };
    match ExportTraceServiceRequest::decode(&bytes[..]) {
        Ok(req) => {
            let n = store_traces(&pool, req).await;
            (StatusCode::OK, format!("ingested {n} spans"))
        }
        Err(e) => {
            crate::selfmon::DROP_DECODE.fetch_add(1, Ordering::Relaxed);
            (
                StatusCode::BAD_REQUEST,
                format!("protobuf decode error: {e}"),
            )
        }
    }
}

// Spanned (unlike ingest_traces): exported spans go to /v1/traces, which is NOT
// spanned, so a metric/log ingest span can't trigger a self-export loop.
#[tracing::instrument(name = "ingest.logs", skip_all)]
pub async fn ingest_logs(
    State(pool): State<PgPool>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    let bytes = match payload(&headers, body) {
        Ok(b) => b,
        Err(e) => return (StatusCode::BAD_REQUEST, format!("gzip error: {e}")),
    };
    match ExportLogsServiceRequest::decode(&bytes[..]) {
        Ok(req) => {
            let n = store_logs(&pool, req).await;
            (StatusCode::OK, format!("ingested {n} logs"))
        }
        Err(e) => {
            crate::selfmon::DROP_DECODE.fetch_add(1, Ordering::Relaxed);
            (
                StatusCode::BAD_REQUEST,
                format!("protobuf decode error: {e}"),
            )
        }
    }
}

#[tracing::instrument(name = "ingest.metrics", skip_all)]
pub async fn ingest_metrics(
    State(pool): State<PgPool>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    let bytes = match payload(&headers, body) {
        Ok(b) => b,
        Err(e) => return (StatusCode::BAD_REQUEST, format!("gzip error: {e}")),
    };
    match ExportMetricsServiceRequest::decode(&bytes[..]) {
        Ok(req) => {
            let n = store_metrics(&pool, req).await;
            (StatusCode::OK, format!("ingested {n} metric points"))
        }
        Err(e) => {
            crate::selfmon::DROP_DECODE.fetch_add(1, Ordering::Relaxed);
            (
                StatusCode::BAD_REQUEST,
                format!("protobuf decode error: {e}"),
            )
        }
    }
}

// ---------------------------------------------------------------------------
// Transport-agnostic storage
// ---------------------------------------------------------------------------

pub async fn store_traces(pool: &PgPool, req: ExportTraceServiceRequest) -> u64 {
    // Decode the whole request into rows first, then write them in batched
    // statements (one per chunk) instead of one INSERT round-trip per span — a
    // large export was N sequential awaited inserts, each taking a pool connection.
    let mut rows: Vec<SpanRow> = Vec::new();
    for rs in &req.resource_spans {
        let rattrs = resource_attrs(rs.resource.as_ref());
        let service = service_name(rattrs);
        for ss in &rs.scope_spans {
            for span in &ss.spans {
                rows.push(span_row(service.as_deref(), rattrs, span));
            }
        }
    }
    let count = write_chunked(pool, &rows, |c, r| Box::pin(insert_spans(c, r))).await;
    crate::selfmon::SPANS_INGESTED.fetch_add(count, Ordering::Relaxed);
    count
}

pub async fn store_logs(pool: &PgPool, req: ExportLogsServiceRequest) -> u64 {
    // Decode the whole request into rows first, then write them in batched
    // statements (one per chunk) instead of one INSERT round-trip per record —
    // the per-row loop was the source of the ~30s ingest p99 (JEF-495).
    let mut rows: Vec<LogRow> = Vec::new();
    for rl in &req.resource_logs {
        // Keep resource attributes (k8s.pod.name / node / container, …) so logs
        // can be filtered by pod/host, not just service.
        let rattrs = resource_attrs(rl.resource.as_ref());
        let service = service_name(rattrs);
        for sl in &rl.scope_logs {
            for rec in &sl.log_records {
                rows.push(log_row(service.as_deref(), rattrs, rec));
            }
        }
    }
    let count = write_chunked(pool, &rows, |c, r| Box::pin(insert_logs(c, r))).await;
    crate::selfmon::LOGS_INGESTED.fetch_add(count, Ordering::Relaxed);
    count
}

/// Rows per batched INSERT. UNNEST binds each column as a single array parameter
/// (so the 65_535 bind-param ceiling is irrelevant), but chunking still bounds the
/// per-statement array size and memory. 5_000 collapses any realistic OTLP batch
/// into one or two round-trips.
const INSERT_CHUNK: usize = 5_000;

/// Bounded retries for a single write when the pooled connection lands on a
/// demoted (read-only) Postgres backend (see [`write_with_failover_retry`]).
const FAILOVER_MAX_ATTEMPTS: u32 = 3;

/// Backoff before each *retry* (not the first attempt), capped at 1s. A Patroni
/// failover promotes a new leader and re-points the `-master` service DNS within a
/// second or two, so a short escalating pause lets a fresh connection re-resolve to
/// the new leader before we give up. Kept small so a transient blip doesn't stall
/// ingest longer than it must. Indexed by (attempt - 1), so it holds exactly one
/// entry per retry — the assertion below couples its length to the attempt bound.
const FAILOVER_BACKOFF: [Duration; 2] = [Duration::from_millis(200), Duration::from_millis(600)];
const _: () = assert!(FAILOVER_BACKOFF.len() == FAILOVER_MAX_ATTEMPTS as usize - 1);

/// True for the transient, connection-scoped errors a Patroni/postgres-operator
/// failover produces — worth retrying on a *fresh* connection, unlike a bad row:
///
/// * `25006` read_only_sql_transaction — the pooled connection's backend was
///   demoted to a read-only replica; the write hits a read-only node (the JEF-496
///   symptom). A new connection re-resolves `-master` DNS to the new leader.
/// * `57P01` admin_shutdown — the old leader terminated the backend on demotion.
///
/// Deliberately narrow: a constraint violation, encoding error, or any other
/// database error is NOT retried — it must fall through to the per-row fallback and
/// DROP_INSERT accounting so a genuinely bad row is never masked by retries.
fn is_failover_error(e: &sqlx::Error) -> bool {
    matches!(
        e,
        sqlx::Error::Database(db) if matches!(db.code().as_deref(), Some("25006") | Some("57P01"))
    )
}

/// A single write's future, boxed with an explicit `Send` bound. `op` builds one of
/// these per attempt from a fresh connection plus the borrowed batch `data`. The box
/// (over the async block) is what keeps `Send` inference tractable — an async closure
/// yielding `&mut PgConnection` otherwise trips "implementation of `Send` is not
/// general enough" once these futures cross the axum/tonic handler boundary. `data` is
/// threaded through a parameter rather than captured by `op`, so `op` borrows nothing
/// and stays a plain `for<'c>` higher-ranked closure.
type WriteFut<'c> =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), sqlx::Error>> + Send + 'c>>;

/// Run one write, retrying on a read-only-failover error against a freshly acquired
/// connection. Each attempt acquires its own connection from `pool` and builds the
/// statement via `op(conn, data)`; on a failover error the connection is
/// dropped-not-recycled (`close_on_drop`) so the pool can't hand the same demoted
/// backend to the next writer, then we back off and retry. A non-failover error
/// returns immediately (no retry) so the caller's per-row fallback + DROP_INSERT
/// accounting still applies. `op` must be a single isolatable statement — it may run
/// more than once.
async fn write_with_failover_retry<D: Sync + ?Sized>(
    pool: &PgPool,
    data: &D,
    op: impl for<'c> Fn(&'c mut PgConnection, &'c D) -> WriteFut<'c>,
) -> Result<(), sqlx::Error> {
    let mut attempt = 0u32;
    loop {
        let mut conn = pool.acquire().await?;
        match op(&mut conn, data).await {
            Ok(()) => return Ok(()),
            Err(e) if is_failover_error(&e) && attempt + 1 < FAILOVER_MAX_ATTEMPTS => {
                // Evict this connection instead of returning it to the pool — it's
                // pinned (TCP) to the demoted, now read-only backend, so recycling it
                // would just hand the same dead node to the next writer. Dropping it
                // forces the pool to open a fresh one that re-resolves `-master` DNS.
                conn.close_on_drop();
                drop(conn);
                tokio::time::sleep(FAILOVER_BACKOFF[attempt as usize]).await;
                attempt += 1;
                tracing::warn!("write hit a read-only/closed backend (failover); retry {attempt}");
            }
            Err(e) => return Err(e),
        }
    }
}

/// Write `rows` in chunks, one batched `INSERT` per chunk. Each write goes through
/// [`write_with_failover_retry`], so a Patroni failover's read-only error is retried
/// on a fresh connection rather than dropped (JEF-496). On a *non-failover* chunk
/// error, fall back to inserting each row of that chunk on its own so a single bad
/// row can't drop the whole batch — every row that still fails is logged and counted
/// in `DROP_INSERT`, preserving the per-row drop accounting the old loop had. If a
/// chunk still fails with a failover error after retries are exhausted, per-row
/// isolation would hit the same read-only wall, so the whole chunk is dropped and
/// counted once rather than multiplying the retry storm. Returns the number of rows
/// written.
async fn write_chunked<T: Sync>(
    pool: &PgPool,
    rows: &[T],
    insert: impl for<'c> Fn(&'c mut PgConnection, &'c [T]) -> WriteFut<'c>,
) -> u64 {
    let mut count = 0u64;
    for chunk in rows.chunks(INSERT_CHUNK) {
        match write_with_failover_retry(pool, chunk, &insert).await {
            // A successful batch counts every row it attempted — matching the old
            // per-row loop, which counted each `execute` (an ON CONFLICT no-op still
            // succeeded and still counted).
            Ok(()) => count += chunk.len() as u64,
            Err(e) if is_failover_error(&e) => {
                // Retries exhausted against a still-read-only backend; per-row would
                // hit the same wall, so drop the chunk and count it once.
                tracing::warn!(
                    "batch insert dropped ({} rows) after failover retries: {e}",
                    chunk.len()
                );
                crate::selfmon::DROP_INSERT.fetch_add(chunk.len() as u64, Ordering::Relaxed);
            }
            Err(e) => {
                tracing::warn!(
                    "batch insert failed ({} rows), isolating per-row: {e}",
                    chunk.len()
                );
                for row in chunk {
                    match write_with_failover_retry(pool, std::slice::from_ref(row), &insert).await
                    {
                        Ok(()) => count += 1,
                        Err(e) => {
                            tracing::warn!("insert row failed: {e}");
                            crate::selfmon::DROP_INSERT.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
            }
        }
    }
    count
}

/// A whole request is buffered in memory before the batched write, so cap how many
/// points one request can contribute — a crafted export could otherwise declare
/// millions of points and exhaust memory. Legitimate collector batches are orders
/// of magnitude smaller; points past the cap are dropped.
const MAX_POINTS_PER_REQUEST: usize = 100_000;

pub async fn store_metrics(pool: &PgPool, req: ExportMetricsServiceRequest) -> u64 {
    // Collect the whole request's points up front, then write them in two batched
    // statements (numbers, histograms) instead of one round-trip per point. A
    // single OTLP export from the collector carries hundreds of points; batching
    // turns hundreds of INSERT round-trips into two.
    let mut nums: Vec<NumRow> = Vec::new();
    let mut hists: Vec<HistRow> = Vec::new();
    for rm in &req.resource_metrics {
        // Resource attributes carry k8s.pod.name / node / container etc. — keep them
        // so metrics are dimensioned, not flat.
        let rattrs = resource_attrs(rm.resource.as_ref());
        let service = service_name(rattrs);
        for sm in &rm.scope_metrics {
            for m in &sm.metrics {
                collect_metric(service.as_deref(), rattrs, m, &mut nums, &mut hists);
            }
        }
    }
    if nums.len() + hists.len() >= MAX_POINTS_PER_REQUEST {
        tracing::warn!(
            "metrics request hit the {MAX_POINTS_PER_REQUEST}-point cap; extra points dropped"
        );
        crate::selfmon::DROP_CAP.fetch_add(1, Ordering::Relaxed);
    }
    // The two flushes hit disjoint rollup rows (different kinds), so run them
    // concurrently rather than one after the other.
    let (n, h) = tokio::join!(flush_numbers(pool, nums), flush_histograms(pool, hists));
    let stored = n + h;
    crate::selfmon::METRIC_POINTS_INGESTED.fetch_add(stored, Ordering::Relaxed);
    stored
}

// ---------------------------------------------------------------------------
// Inserts
// ---------------------------------------------------------------------------

/// One decoded span, ready to batch-insert. Mirrors the `spans` columns.
struct SpanRow {
    trace_id: String,
    span_id: String,
    parent_span_id: Option<String>,
    service: Option<String>,
    name: String,
    kind: i32,
    start_time: DateTime<Utc>,
    end_time: DateTime<Utc>,
    duration_ms: f64,
    status_code: Option<i32>,
    status_message: Option<String>,
    attrs: serde_json::Value,
}

/// One decoded log record, ready to batch-insert. Mirrors the `logs` columns.
struct LogRow {
    time: DateTime<Utc>,
    trace_id: Option<String>,
    span_id: Option<String>,
    service: Option<String>,
    severity_number: i32,
    severity_text: String,
    body: Option<String>,
    attrs: serde_json::Value,
}

/// Decode one span into a `SpanRow`. No DB I/O — `store_traces` batches these.
fn span_row(service: Option<&str>, resource: &[KeyValue], span: &Span) -> SpanRow {
    let (status_code, status_message) = match &span.status {
        Some(s) => (
            Some(s.code),
            (!s.message.is_empty()).then(|| s.message.clone()),
        ),
        None => (None, None),
    };
    SpanRow {
        trace_id: hex::encode(&span.trace_id),
        span_id: hex::encode(&span.span_id),
        parent_span_id: (!span.parent_span_id.is_empty())
            .then(|| hex::encode(&span.parent_span_id)),
        service: service.map(str::to_string),
        name: span.name.clone(),
        kind: span.kind,
        start_time: ts(span.start_time_unix_nano),
        end_time: ts(span.end_time_unix_nano),
        duration_ms: span
            .end_time_unix_nano
            .saturating_sub(span.start_time_unix_nano) as f64
            / 1_000_000.0,
        status_code,
        status_message,
        attrs: merged_attrs(resource, &span.attributes),
    }
}

/// Decode one log record into a `LogRow`. No DB I/O — `store_logs` batches these.
fn log_row(service: Option<&str>, resource: &[KeyValue], rec: &LogRecord) -> LogRow {
    let nanos = if rec.time_unix_nano != 0 {
        rec.time_unix_nano
    } else {
        rec.observed_time_unix_nano
    };
    LogRow {
        time: ts(nanos),
        trace_id: (!rec.trace_id.is_empty()).then(|| hex::encode(&rec.trace_id)),
        span_id: (!rec.span_id.is_empty()).then(|| hex::encode(&rec.span_id)),
        service: service.map(str::to_string),
        severity_number: rec.severity_number,
        severity_text: rec.severity_text.clone(),
        body: rec.body.as_ref().map(any_value_to_text),
        attrs: merged_attrs(resource, &rec.attributes),
    }
}

/// Batch-insert spans: one `INSERT … SELECT * FROM unnest(...)` over parallel
/// per-column arrays (all bound params — no string interpolation). `ON CONFLICT
/// (trace_id, span_id) DO NOTHING` de-dupes against existing rows and intra-batch
/// duplicates alike, matching the old single-row insert. One `execute` per call,
/// against a caller-supplied connection so `write_with_failover_retry` can rerun it
/// on a fresh connection after a read-only-failover error.
async fn insert_spans(conn: &mut PgConnection, rows: &[SpanRow]) -> Result<(), sqlx::Error> {
    // Borrow each column into a parallel array; the row slice outlives the query,
    // so no per-row cloning is needed.
    let trace_ids: Vec<&str> = rows.iter().map(|r| r.trace_id.as_str()).collect();
    let span_ids: Vec<&str> = rows.iter().map(|r| r.span_id.as_str()).collect();
    let parents: Vec<Option<&str>> = rows.iter().map(|r| r.parent_span_id.as_deref()).collect();
    let services: Vec<Option<&str>> = rows.iter().map(|r| r.service.as_deref()).collect();
    let names: Vec<&str> = rows.iter().map(|r| r.name.as_str()).collect();
    let kinds: Vec<i32> = rows.iter().map(|r| r.kind).collect();
    let starts: Vec<DateTime<Utc>> = rows.iter().map(|r| r.start_time).collect();
    let ends: Vec<DateTime<Utc>> = rows.iter().map(|r| r.end_time).collect();
    let durations: Vec<f64> = rows.iter().map(|r| r.duration_ms).collect();
    let status_codes: Vec<Option<i32>> = rows.iter().map(|r| r.status_code).collect();
    let status_msgs: Vec<Option<&str>> = rows.iter().map(|r| r.status_message.as_deref()).collect();
    let attrs: Vec<&serde_json::Value> = rows.iter().map(|r| &r.attrs).collect();

    sqlx::query(
        "INSERT INTO spans (trace_id, span_id, parent_span_id, service, name, kind,
             start_time, end_time, duration_ms, status_code, status_message, attributes)
         SELECT * FROM unnest($1::text[], $2::text[], $3::text[], $4::text[], $5::text[],
                              $6::int[], $7::timestamptz[], $8::timestamptz[], $9::float8[],
                              $10::int[], $11::text[], $12::jsonb[])
         ON CONFLICT (trace_id, span_id) DO NOTHING",
    )
    .bind(&trace_ids)
    .bind(&span_ids)
    .bind(&parents)
    .bind(&services)
    .bind(&names)
    .bind(&kinds)
    .bind(&starts)
    .bind(&ends)
    .bind(&durations)
    .bind(&status_codes)
    .bind(&status_msgs)
    .bind(&attrs)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Batch-insert logs: one `INSERT … SELECT * FROM unnest(...)` over parallel
/// per-column arrays (all bound params). One `execute` per call, against a
/// caller-supplied connection so `write_with_failover_retry` can rerun it on a fresh
/// connection after a read-only-failover error.
async fn insert_logs(conn: &mut PgConnection, rows: &[LogRow]) -> Result<(), sqlx::Error> {
    let times: Vec<DateTime<Utc>> = rows.iter().map(|r| r.time).collect();
    let trace_ids: Vec<Option<&str>> = rows.iter().map(|r| r.trace_id.as_deref()).collect();
    let span_ids: Vec<Option<&str>> = rows.iter().map(|r| r.span_id.as_deref()).collect();
    let services: Vec<Option<&str>> = rows.iter().map(|r| r.service.as_deref()).collect();
    let sev_nums: Vec<i32> = rows.iter().map(|r| r.severity_number).collect();
    let sev_texts: Vec<&str> = rows.iter().map(|r| r.severity_text.as_str()).collect();
    let bodies: Vec<Option<&str>> = rows.iter().map(|r| r.body.as_deref()).collect();
    let attrs: Vec<&serde_json::Value> = rows.iter().map(|r| &r.attrs).collect();

    sqlx::query(
        "INSERT INTO logs (time, trace_id, span_id, service, severity_number, severity_text, body, attributes)
         SELECT * FROM unnest($1::timestamptz[], $2::text[], $3::text[], $4::text[],
                              $5::int[], $6::text[], $7::text[], $8::jsonb[])",
    )
    .bind(&times)
    .bind(&trace_ids)
    .bind(&span_ids)
    .bind(&services)
    .bind(&sev_nums)
    .bind(&sev_texts)
    .bind(&bodies)
    .bind(&attrs)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// One decoded numeric (gauge / sum) point, ready to batch-insert.
struct NumRow {
    time: DateTime<Utc>,
    service: Option<String>,
    name: String,
    kind: &'static str,
    value: f64,
    unit: Option<String>,
    is_monotonic: Option<bool>,
    attrs: serde_json::Value,
    /// One exemplar trace/span id (JEF-433), picked from the point's exemplars —
    /// `None` for points that carry no sampled exemplar (the common case).
    exemplar_trace_id: Option<String>,
    exemplar_span_id: Option<String>,
}

/// One decoded histogram point, ready to batch-insert.
struct HistRow {
    time: DateTime<Utc>,
    service: Option<String>,
    name: String,
    unit: Option<String>,
    attrs: serde_json::Value,
    sum: Option<f64>,
    count: i64,
    bounds: Vec<f64>,
    counts: Vec<i64>,
    /// One exemplar trace/span id (JEF-433); see `NumRow`.
    exemplar_trace_id: Option<String>,
    exemplar_span_id: Option<String>,
}

/// Decode one metric's data points into the per-request batches. No DB I/O here —
/// `store_metrics` flushes the accumulated batches once at the end.
fn collect_metric(
    service: Option<&str>,
    resource: &[KeyValue],
    m: &Metric,
    nums: &mut Vec<NumRow>,
    hists: &mut Vec<HistRow>,
) {
    let unit = (!m.unit.is_empty()).then(|| m.unit.clone());
    let full = |nums: &Vec<NumRow>, hists: &Vec<HistRow>| {
        nums.len() + hists.len() >= MAX_POINTS_PER_REQUEST
    };
    match &m.data {
        Some(metric::Data::Gauge(g)) => {
            for dp in &g.data_points {
                if full(nums, hists) {
                    break;
                }
                if let Some(row) = num_row(service, resource, m, "gauge", &unit, None, dp) {
                    nums.push(row);
                }
            }
        }
        Some(metric::Data::Sum(s)) => {
            // is_monotonic distinguishes a counter (rate it) from an
            // UpDownCounter (a gauge-like running value).
            for dp in &s.data_points {
                if full(nums, hists) {
                    break;
                }
                if let Some(row) =
                    num_row(service, resource, m, "sum", &unit, Some(s.is_monotonic), dp)
                {
                    nums.push(row);
                }
            }
        }
        Some(metric::Data::Histogram(h)) => {
            for dp in &h.data_points {
                if full(nums, hists) {
                    break;
                }
                let (exemplar_trace_id, exemplar_span_id) = first_exemplar(&dp.exemplars);
                hists.push(HistRow {
                    time: ts(dp.time_unix_nano),
                    service: service.map(str::to_string),
                    name: m.name.clone(),
                    unit: unit.clone(),
                    attrs: merged_attrs(resource, &dp.attributes),
                    sum: dp.sum,
                    count: dp.count as i64,
                    // bucket_counts has one more entry than explicit_bounds (+Inf).
                    bounds: dp.explicit_bounds.clone(),
                    counts: dp.bucket_counts.iter().map(|&c| c as i64).collect(),
                    exemplar_trace_id,
                    exemplar_span_id,
                });
            }
        }
        // Exponential histograms and summaries aren't stored in v0.
        _ => {}
    }
}

/// Decode one numeric data point into a `NumRow`, or `None` if it carries no value.
fn num_row(
    service: Option<&str>,
    resource: &[KeyValue],
    m: &Metric,
    kind: &'static str,
    unit: &Option<String>,
    is_monotonic: Option<bool>,
    dp: &NumberDataPoint,
) -> Option<NumRow> {
    let value = match dp.value {
        Some(number_data_point::Value::AsDouble(d)) => d,
        Some(number_data_point::Value::AsInt(i)) => i as f64,
        None => return None,
    };
    let (exemplar_trace_id, exemplar_span_id) = first_exemplar(&dp.exemplars);
    Some(NumRow {
        time: ts(dp.time_unix_nano),
        service: service.map(str::to_string),
        name: m.name.clone(),
        kind,
        value,
        unit: unit.clone(),
        is_monotonic,
        attrs: merged_attrs(resource, &dp.attributes),
        exemplar_trace_id,
        exemplar_span_id,
    })
}

/// Pick one exemplar to keep per data point (JEF-433): the first exemplar that
/// actually carries a trace id — an exemplar's span/trace ids are optional in the
/// OTLP spec (absent when the measurement wasn't recorded inside a sampled trace),
/// so a point can have exemplars with no usable id. `metrics` keeps at most one
/// exemplar per row (not the full list) — enough to link a chart point to *a*
/// concrete trace without a separate exemplars table.
fn first_exemplar(exemplars: &[Exemplar]) -> (Option<String>, Option<String>) {
    match exemplars.iter().find(|e| !e.trace_id.is_empty()) {
        Some(e) => (
            Some(hex::encode(&e.trace_id)),
            (!e.span_id.is_empty()).then(|| hex::encode(&e.span_id)),
        ),
        None => (None, None),
    }
}

/// One request's numeric points as the parallel per-column arrays sqlx's `unnest`
/// binds, plus the rollup bucket width. Bundling them lets the batch travel through
/// [`write_with_failover_retry`] as borrowed `data` (so its `op` captures nothing).
struct NumBatch {
    times: Vec<DateTime<Utc>>,
    services: Vec<Option<String>>,
    names: Vec<String>,
    kinds: Vec<&'static str>,
    values: Vec<f64>,
    units: Vec<Option<String>>,
    attrs: Vec<serde_json::Value>,
    monos: Vec<Option<bool>>,
    exemplar_trace_ids: Vec<Option<String>>,
    exemplar_span_ids: Vec<Option<String>>,
    width: f64,
}

/// The single aggregating statement for a numeric batch — see [`flush_numbers`].
/// One `execute` per call, against a caller-supplied connection so
/// `write_with_failover_retry` can rerun it on a fresh connection after a failover.
async fn insert_numbers(conn: &mut PgConnection, b: &NumBatch) -> Result<(), sqlx::Error> {
    // Exemplars are per-raw-point only — deliberately absent from the rollup
    // INSERT/UPDATE below (ADR 0011: rollups aggregate the dimension away, same
    // as the existing constraint on bucket_bounds/bucket_counts).
    sqlx::query(
        "WITH pts AS (
             SELECT * FROM unnest($1::timestamptz[], $2::text[], $3::text[], $4::text[],
                                  $5::float8[], $6::text[], $7::jsonb[], $8::bool[],
                                  $9::text[], $10::text[])
                 AS t(time, service, name, kind, value, unit, attrs, is_monotonic,
                      exemplar_trace_id, exemplar_span_id)
         ),
         raw AS (
             INSERT INTO metrics (time, service, name, kind, value, unit, attributes, is_monotonic,
                                  exemplar_trace_id, exemplar_span_id)
             SELECT time, service, name, kind, value, unit, attrs, is_monotonic,
                    exemplar_trace_id, exemplar_span_id FROM pts
         )
         INSERT INTO metric_series_rollups
             (bucket, name, series_key, attrs, service, kind, unit, is_monotonic,
              count, sum, min, max, avg)
         SELECT metric_bucket(time, $11),
                name, metric_series_key(service, attrs), attrs, service, kind, unit,
                is_monotonic, count(*), sum(value), min(value), max(value), avg(value)
         FROM pts
         GROUP BY 1, 2, 3, 4, 5, 6, 7, 8
         -- Lock the conflict rows in a fixed (name, series_key, bucket) order so
         -- concurrent ingest batches can't deadlock acquiring the same hot
         -- current-bucket rows in different orders.
         ORDER BY 2, 3, 1
         ON CONFLICT (name, series_key, bucket) DO UPDATE SET
             count = metric_series_rollups.count + EXCLUDED.count,
             sum   = metric_series_rollups.sum + EXCLUDED.sum,
             min   = least(metric_series_rollups.min, EXCLUDED.min),
             max   = greatest(metric_series_rollups.max, EXCLUDED.max),
             avg   = (metric_series_rollups.sum + EXCLUDED.sum)
                     / (metric_series_rollups.count + EXCLUDED.count),
             unit  = EXCLUDED.unit,
             is_monotonic = EXCLUDED.is_monotonic",
    )
    .bind(&b.times)
    .bind(&b.services)
    .bind(&b.names)
    .bind(&b.kinds)
    .bind(&b.values)
    .bind(&b.units)
    .bind(&b.attrs)
    .bind(&b.monos)
    .bind(&b.exemplar_trace_ids)
    .bind(&b.exemplar_span_ids)
    .bind(b.width)
    .execute(&mut *conn)
    .await
    .map(|_| ())
}

/// Batch-insert all numeric points from one request. Mirrors the per-point
/// aggregate-on-insert (keep the raw point AND fold it into its live rollup
/// bucket) but over the whole batch in a single statement: `unnest` expands the
/// parallel arrays, a data-modifying CTE writes the raw rows, then a GROUP BY
/// pre-aggregates points that share a (name, series_key, bucket) so the
/// ON CONFLICT upsert never touches the same rollup row twice. Returns the number
/// of raw points written (0 on error or empty batch).
async fn flush_numbers(pool: &PgPool, rows: Vec<NumRow>) -> u64 {
    let n = rows.len();
    if n == 0 {
        return 0;
    }
    // One move pass into the parallel arrays sqlx's `unnest` binds — owned fields
    // (attrs, service, …) move out of each row rather than being cloned.
    let mut b = NumBatch {
        times: Vec::with_capacity(n),
        services: Vec::with_capacity(n),
        names: Vec::with_capacity(n),
        kinds: Vec::with_capacity(n),
        values: Vec::with_capacity(n),
        units: Vec::with_capacity(n),
        attrs: Vec::with_capacity(n),
        monos: Vec::with_capacity(n),
        exemplar_trace_ids: Vec::with_capacity(n),
        exemplar_span_ids: Vec::with_capacity(n),
        width: rollup_width(),
    };
    for r in rows {
        b.times.push(r.time);
        b.services.push(r.service);
        b.names.push(r.name);
        b.kinds.push(r.kind);
        b.values.push(r.value);
        b.units.push(r.unit);
        b.attrs.push(r.attrs);
        b.monos.push(r.is_monotonic);
        b.exemplar_trace_ids.push(r.exemplar_trace_id);
        b.exemplar_span_ids.push(r.exemplar_span_id);
    }
    // The whole-batch write goes through failover retry (JEF-496): a Patroni failover's
    // read-only error is retried on a fresh connection, not dropped. There's no per-row
    // fallback here (a metrics batch is one aggregating statement), so on a persistent
    // error — failover retries exhausted or any other DB error — the batch drops and
    // every point counts in DROP_INSERT, exactly as before.
    match write_with_failover_retry(pool, &b, |c, b| Box::pin(insert_numbers(c, b))).await {
        Ok(()) => n as u64,
        Err(e) => {
            tracing::warn!("batch insert metrics failed: {e}");
            crate::selfmon::DROP_INSERT.fetch_add(n as u64, Ordering::Relaxed);
            0
        }
    }
}

/// Batch-insert all histogram points from one request. Same shape as
/// `flush_numbers`, but the per-point bucket arrays are variable-length, so the
/// batch travels as a JSONB array (one object per point) that `jsonb_array_elements`
/// expands and casts back to `float8[]`/`bigint[]`. The rollup fold sums
/// bucket_counts element-wise via the `array_sum` aggregate. Returns the number of
/// points written (0 on error or empty batch).
async fn flush_histograms(pool: &PgPool, rows: Vec<HistRow>) -> u64 {
    let n = rows.len();
    if n == 0 {
        return 0;
    }
    // Move each row's fields into the JSONB payload rather than cloning them.
    let payload = serde_json::Value::Array(
        rows.into_iter()
            .map(|r| {
                json!({
                    "time": r.time,
                    "service": r.service,
                    "name": r.name,
                    "unit": r.unit,
                    "attrs": r.attrs,
                    "sum": r.sum,
                    "count": r.count,
                    "bounds": r.bounds,
                    "counts": r.counts,
                    "exemplar_trace_id": r.exemplar_trace_id,
                    "exemplar_span_id": r.exemplar_span_id,
                })
            })
            .collect(),
    );
    // Whole-batch write through failover retry (JEF-496); see flush_numbers for the
    // drop/accounting rationale. The JSONB payload + bucket width travel as borrowed
    // `data` so the retry op captures nothing.
    let b = HistBatch {
        payload,
        width: rollup_width(),
    };
    match write_with_failover_retry(pool, &b, |c, b| Box::pin(insert_histograms(c, b))).await {
        Ok(()) => n as u64,
        Err(e) => {
            tracing::warn!("batch insert histograms failed: {e}");
            crate::selfmon::DROP_INSERT.fetch_add(n as u64, Ordering::Relaxed);
            0
        }
    }
}

/// One request's histogram points as a JSONB array (one object per point) plus the
/// rollup bucket width. Borrowed as `data` by [`write_with_failover_retry`].
struct HistBatch {
    payload: serde_json::Value,
    width: f64,
}

/// The single aggregating statement for a histogram batch — see [`flush_histograms`].
/// One `execute` per call, against a caller-supplied connection so the failover retry
/// can rerun it on a fresh connection.
async fn insert_histograms(conn: &mut PgConnection, b: &HistBatch) -> Result<(), sqlx::Error> {
    // Exemplars are per-raw-point only — the rollup INSERT/UPDATE below doesn't
    // carry them; see insert_numbers.
    sqlx::query(
        "WITH pts AS (
             SELECT (e->>'time')::timestamptz AS time,
                    e->>'service' AS service,
                    e->>'name' AS name,
                    e->>'unit' AS unit,
                    e->'attrs' AS attrs,
                    (e->>'sum')::float8 AS sum,
                    (e->>'count')::bigint AS count,
                    (SELECT array_agg(x::float8) FROM jsonb_array_elements_text(e->'bounds') x) AS bounds,
                    (SELECT array_agg(x::bigint) FROM jsonb_array_elements_text(e->'counts') x) AS counts,
                    e->>'exemplar_trace_id' AS exemplar_trace_id,
                    e->>'exemplar_span_id' AS exemplar_span_id
             FROM jsonb_array_elements($1::jsonb) e
         ),
         raw AS (
             INSERT INTO metrics
                 (time, service, name, kind, value, count, unit, attributes, bucket_bounds, bucket_counts,
                  exemplar_trace_id, exemplar_span_id)
             SELECT time, service, name, 'histogram', sum, count, unit, attrs, bounds, counts,
                    exemplar_trace_id, exemplar_span_id FROM pts
         )
         INSERT INTO metric_series_rollups
             (bucket, name, series_key, attrs, service, kind, unit,
              count, sum, min, max, avg, bucket_bounds, bucket_counts)
         SELECT metric_bucket(time, $2),
                name, metric_series_key(service, attrs), attrs, service, 'histogram', unit,
                sum(count), sum(sum), min(sum), max(sum), sum(sum) / nullif(sum(count), 0),
                min(bounds), array_sum(counts)
         FROM pts
         GROUP BY 1, 2, 3, 4, 5, 7
         -- Fixed conflict-row lock order; see flush_numbers.
         ORDER BY 2, 3, 1
         ON CONFLICT (name, series_key, bucket) DO UPDATE SET
             count = metric_series_rollups.count + EXCLUDED.count,
             sum   = metric_series_rollups.sum + EXCLUDED.sum,
             min   = least(metric_series_rollups.min, EXCLUDED.min),
             max   = greatest(metric_series_rollups.max, EXCLUDED.max),
             avg   = (metric_series_rollups.sum + EXCLUDED.sum)
                     / nullif(metric_series_rollups.count + EXCLUDED.count, 0),
             unit  = EXCLUDED.unit,
             bucket_bounds = coalesce(metric_series_rollups.bucket_bounds, EXCLUDED.bucket_bounds),
             bucket_counts = array_add(metric_series_rollups.bucket_counts, EXCLUDED.bucket_counts)",
    )
    .bind(&b.payload)
    .bind(b.width)
    .execute(&mut *conn)
    .await
    .map(|_| ())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn ts(nanos: u64) -> DateTime<Utc> {
    // OTLP carries u64 nanoseconds since the epoch, but from_timestamp_nanos takes
    // an i64. Values past i64::MAX (year ~2262) are implausible or hostile — a bare
    // `as i64` cast would wrap them to a far-past time that the retention sweep then
    // silently prunes. Fall back to receive time rather than store a garbage stamp.
    match i64::try_from(nanos) {
        Ok(n) => DateTime::from_timestamp_nanos(n),
        Err(_) => Utc::now(),
    }
}

/// Rollup bucket width in seconds (`WATCHER_ROLLUP_BUCKET_SECS`, default 300),
/// read once. Must match api.rs' `rollup_bucket_secs` so buckets line up.
fn rollup_width() -> f64 {
    static W: std::sync::OnceLock<f64> = std::sync::OnceLock::new();
    *W.get_or_init(|| {
        std::env::var("WATCHER_ROLLUP_BUCKET_SECS")
            .ok()
            .and_then(|s| s.parse::<f64>().ok())
            .filter(|s| *s > 0.0)
            .unwrap_or(300.0)
    })
}

fn resource_attrs(
    resource: Option<&opentelemetry_proto::tonic::resource::v1::Resource>,
) -> &[KeyValue] {
    resource.map(|r| r.attributes.as_slice()).unwrap_or(&[])
}

fn string_attr<'a>(attrs: &'a [KeyValue], key: &str) -> Option<&'a str> {
    attrs
        .iter()
        .find(|kv| kv.key == key)
        .and_then(|kv| kv.value.as_ref())
        .and_then(|v| match &v.value {
            Some(any_value::Value::StringValue(s)) => Some(s.as_str()),
            _ => None,
        })
}

/// Resolve a service name for stored telemetry. Prefers an explicit
/// `service.name`, but the OTel SDKs default it to `unknown_service[:exe]` when
/// the app set none — in that case fall back to a meaningful k8s identity
/// (deployment/pod/…), then the configured `WATCHER_DEFAULT_SERVICE`.
fn service_name(attrs: &[KeyValue]) -> Option<String> {
    if let Some(s) = string_attr(attrs, "service.name") {
        if !s.is_empty() && !s.starts_with("unknown_service") {
            return Some(s.to_string());
        }
    }
    for key in [
        "k8s.deployment.name",
        "k8s.statefulset.name",
        "k8s.daemonset.name",
        "k8s.cronjob.name",
        "k8s.pod.name",
    ] {
        if let Some(s) = string_attr(attrs, key) {
            if !s.is_empty() {
                return Some(s.to_string());
            }
        }
    }
    default_service().clone()
}

/// Configured fallback service name (`WATCHER_DEFAULT_SERVICE`), read once.
fn default_service() -> &'static Option<String> {
    static D: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    D.get_or_init(|| {
        std::env::var("WATCHER_DEFAULT_SERVICE")
            .ok()
            .filter(|s| !s.is_empty())
    })
}

/// Cap on OTLP attribute nesting. A sender (hostile or buggy) can nest
/// ArrayValue/KvlistValue arbitrarily deep; the conversion recurses once per
/// level, so without a bound a single small message could overflow the stack and
/// crash the (unauthenticated, in-cluster) ingest path. Past the cap the
/// over-nested subtree is dropped to Null — 32 is far beyond any real attribute.
const MAX_ATTR_DEPTH: usize = 32;

/// Resource attributes (k8s.pod.name / node / container, …) overlaid with a data
/// point's own attributes — so stored metrics keep their dimensions. The point's
/// attributes win on a key collision.
fn merged_attrs(resource: &[KeyValue], point: &[KeyValue]) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    for kv in resource.iter().chain(point.iter()) {
        if let Some(v) = &kv.value {
            map.insert(kv.key.clone(), any_value_to_json_at(v, 0));
        }
    }
    serde_json::Value::Object(map)
}

fn kvlist_to_json(attrs: &[KeyValue], depth: usize) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    for kv in attrs {
        if let Some(v) = &kv.value {
            map.insert(kv.key.clone(), any_value_to_json_at(v, depth));
        }
    }
    serde_json::Value::Object(map)
}

fn any_value_to_json(v: &AnyValue) -> serde_json::Value {
    any_value_to_json_at(v, 0)
}

fn any_value_to_json_at(v: &AnyValue, depth: usize) -> serde_json::Value {
    use any_value::Value as V;
    match &v.value {
        Some(V::StringValue(s)) => json!(s),
        // OTLP 0.32 added a string-table reference (index into a shared string
        // table). We don't thread that table into this converter, and standard
        // SDKs/collectors send inline StringValue, so drop the ref to Null.
        Some(V::StringValueStrindex(_)) => serde_json::Value::Null,
        Some(V::BoolValue(b)) => json!(b),
        Some(V::IntValue(i)) => json!(i),
        Some(V::DoubleValue(d)) => json!(d),
        Some(V::BytesValue(b)) => json!(hex::encode(b)),
        // Stop recursing past the depth cap: drop the over-nested container.
        Some(V::ArrayValue(_) | V::KvlistValue(_)) if depth >= MAX_ATTR_DEPTH => {
            serde_json::Value::Null
        }
        Some(V::ArrayValue(a)) => serde_json::Value::Array(
            a.values
                .iter()
                .map(|x| any_value_to_json_at(x, depth + 1))
                .collect(),
        ),
        Some(V::KvlistValue(kv)) => kvlist_to_json(&kv.values, depth + 1),
        None => serde_json::Value::Null,
    }
}

fn any_value_to_text(v: &AnyValue) -> String {
    match &v.value {
        Some(any_value::Value::StringValue(s)) => s.clone(),
        _ => any_value_to_json(v).to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry_proto::tonic::common::v1::{any_value::Value as V, ArrayValue, KeyValueList};

    fn sval(s: &str) -> AnyValue {
        AnyValue {
            value: Some(V::StringValue(s.to_string())),
        }
    }

    #[test]
    fn ts_converts_nanos() {
        // 1_500_000_000 ns = 1.5 s after the epoch.
        let t = ts(1_500_000_000);
        assert_eq!(t.timestamp(), 1);
        assert_eq!(t.timestamp_subsec_nanos(), 500_000_000);
    }

    #[test]
    fn ts_clamps_implausible_nanos() {
        // u64::MAX nanos wraps to a 1969 timestamp via a bare `as i64`; the guard
        // falls back to receive time (well past 2020) instead of a far-past stamp.
        assert!(ts(u64::MAX).timestamp() > 1_600_000_000);
    }

    #[test]
    fn attrs_to_json_caps_deep_nesting() {
        // Nest ArrayValue far past MAX_ATTR_DEPTH. Conversion must not overflow the
        // stack, and the over-nested subtree is dropped to Null at the cap.
        let mut v = sval("leaf");
        for _ in 0..1000 {
            v = AnyValue {
                value: Some(V::ArrayValue(ArrayValue { values: vec![v] })),
            };
        }
        let json = any_value_to_json(&v); // does not panic / overflow
        let (mut cur, mut depth) = (&json, 0);
        while let serde_json::Value::Array(arr) = cur {
            assert_eq!(arr.len(), 1);
            cur = &arr[0];
            depth += 1;
        }
        assert!(cur.is_null(), "deep nesting should truncate to Null");
        assert_eq!(depth, MAX_ATTR_DEPTH);
    }

    #[test]
    fn service_name_extraction() {
        let attrs = vec![
            KeyValue {
                key: "host".into(),
                value: Some(sval("box")),
                ..Default::default()
            },
            KeyValue {
                key: "service.name".into(),
                value: Some(sval("checkout")),
                ..Default::default()
            },
        ];
        assert_eq!(service_name(&attrs).as_deref(), Some("checkout"));
        assert_eq!(service_name(&[]), None);
    }

    #[test]
    fn service_name_falls_back_past_unknown() {
        // SDK default "unknown_service:foo" → fall back to a k8s identity.
        let attrs = vec![
            KeyValue {
                key: "service.name".into(),
                value: Some(sval("unknown_service:node")),
                ..Default::default()
            },
            KeyValue {
                key: "k8s.deployment.name".into(),
                value: Some(sval("checkout")),
                ..Default::default()
            },
        ];
        assert_eq!(service_name(&attrs).as_deref(), Some("checkout"));

        // unknown_service with no k8s hints and no configured default → None.
        let bare = vec![KeyValue {
            key: "service.name".into(),
            value: Some(sval("unknown_service")),
            ..Default::default()
        }];
        assert_eq!(service_name(&bare), None);
    }

    #[test]
    fn service_name_ignores_non_string() {
        let attrs = vec![KeyValue {
            key: "service.name".into(),
            value: Some(AnyValue {
                value: Some(V::IntValue(7)),
            }),
            ..Default::default()
        }];
        assert_eq!(service_name(&attrs), None);
    }

    #[test]
    fn attrs_to_json_covers_all_value_kinds() {
        let attrs = vec![
            KeyValue {
                key: "s".into(),
                value: Some(sval("x")),
                ..Default::default()
            },
            KeyValue {
                key: "b".into(),
                value: Some(AnyValue {
                    value: Some(V::BoolValue(true)),
                }),
                ..Default::default()
            },
            KeyValue {
                key: "i".into(),
                value: Some(AnyValue {
                    value: Some(V::IntValue(42)),
                }),
                ..Default::default()
            },
            KeyValue {
                key: "d".into(),
                value: Some(AnyValue {
                    value: Some(V::DoubleValue(1.5)),
                }),
                ..Default::default()
            },
            KeyValue {
                key: "by".into(),
                value: Some(AnyValue {
                    value: Some(V::BytesValue(vec![0xde, 0xad])),
                }),
                ..Default::default()
            },
            KeyValue {
                key: "arr".into(),
                value: Some(AnyValue {
                    value: Some(V::ArrayValue(ArrayValue {
                        values: vec![sval("a"), sval("b")],
                    })),
                }),
                ..Default::default()
            },
            KeyValue {
                key: "kv".into(),
                value: Some(AnyValue {
                    value: Some(V::KvlistValue(KeyValueList {
                        values: vec![KeyValue {
                            key: "nested".into(),
                            value: Some(sval("y")),
                            ..Default::default()
                        }],
                    })),
                }),
                ..Default::default()
            },
        ];
        let json = kvlist_to_json(&attrs, 0);
        assert_eq!(json["s"], "x");
        assert_eq!(json["b"], true);
        assert_eq!(json["i"], 42);
        assert_eq!(json["d"], 1.5);
        assert_eq!(json["by"], "dead"); // hex-encoded
        assert_eq!(json["arr"], serde_json::json!(["a", "b"]));
        assert_eq!(json["kv"]["nested"], "y");
    }

    #[test]
    fn any_value_to_text_passthrough_and_fallback() {
        assert_eq!(any_value_to_text(&sval("plain")), "plain");
        let n = AnyValue {
            value: Some(V::IntValue(5)),
        };
        // Non-string bodies are JSON-stringified.
        assert_eq!(any_value_to_text(&n), "5");
    }

    // --- JEF-496: read-only-failover write retry -------------------------------
    //
    // These exercise `write_with_failover_retry` against a real Postgres (CI's
    // service container; skipped when DATABASE_URL is unset). They don't rely on
    // durable row state — a concurrent test binary's TRUNCATE is harmless — only on
    // the retry semantics: the attempt count and Ok/Err outcome.

    use std::sync::atomic::AtomicU32;

    async fn pool_or_skip() -> Option<PgPool> {
        let url = std::env::var("DATABASE_URL").ok()?;
        let pool = crate::db::connect(&url).await.expect("connect");
        crate::db::migrate(&pool).await.expect("migrate");
        Some(pool)
    }

    #[tokio::test]
    async fn failover_retry_recovers_from_read_only() {
        let Some(pool) = pool_or_skip().await else {
            eprintln!("skipping: DATABASE_URL not set");
            return;
        };
        // Simulate a Patroni failover: the first connection the helper acquires is
        // demoted to read-only (SET default_transaction_read_only = on is a
        // session-scoped setting, so the next statement on it is read-only), so the
        // INSERT fails with 25006 — exactly the dropped-write symptom. The helper
        // evicts that connection and retries on a fresh one (server default:
        // read-write), which succeeds. Zero drops, one recovery.
        let attempts = AtomicU32::new(0);
        let res = write_with_failover_retry(&pool, &attempts, |conn, attempts| {
            Box::pin(async move {
                let a = attempts.fetch_add(1, Ordering::SeqCst);
                if a == 0 {
                    sqlx::query("SET default_transaction_read_only = on")
                        .execute(&mut *conn)
                        .await?;
                }
                sqlx::query("INSERT INTO logs (time, service, body) VALUES (now(), 'jef496', 'x')")
                    .execute(&mut *conn)
                    .await
                    .map(|_| ())
            })
        })
        .await;

        assert!(
            res.is_ok(),
            "read-only write must recover on retry: {res:?}"
        );
        assert_eq!(
            attempts.load(Ordering::SeqCst),
            2,
            "failed once (25006), retried once on a fresh connection, then succeeded"
        );
    }

    #[tokio::test]
    async fn failover_retry_does_not_retry_other_errors() {
        let Some(pool) = pool_or_skip().await else {
            eprintln!("skipping: DATABASE_URL not set");
            return;
        };
        // A non-failover error (division by zero, SQLSTATE 22012) is a stand-in for a
        // bad row: it must surface immediately, not be retried, so the caller's
        // per-row fallback + DROP_INSERT accounting still applies.
        let attempts = AtomicU32::new(0);
        let res = write_with_failover_retry(&pool, &attempts, |conn, attempts| {
            Box::pin(async move {
                attempts.fetch_add(1, Ordering::SeqCst);
                sqlx::query("SELECT 1 / 0")
                    .execute(&mut *conn)
                    .await
                    .map(|_| ())
            })
        })
        .await;

        assert!(res.is_err(), "a non-25006 error must surface, not retry");
        assert!(
            !is_failover_error(&res.unwrap_err()),
            "the error is correctly classified as non-failover"
        );
        assert_eq!(
            attempts.load(Ordering::SeqCst),
            1,
            "a non-failover error is tried exactly once"
        );
    }
}
