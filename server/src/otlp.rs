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
    metrics::v1::{metric, number_data_point, HistogramDataPoint, Metric, NumberDataPoint},
    trace::v1::Span,
};
use prost::Message;
use serde_json::json;
use sqlx::PgPool;
use std::io::Read;

// ---------------------------------------------------------------------------
// HTTP entrypoints
// ---------------------------------------------------------------------------

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
        flate2::read::GzDecoder::new(&body[..]).read_to_end(&mut out)?;
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
        Err(e) => (
            StatusCode::BAD_REQUEST,
            format!("protobuf decode error: {e}"),
        ),
    }
}

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
        Err(e) => (
            StatusCode::BAD_REQUEST,
            format!("protobuf decode error: {e}"),
        ),
    }
}

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
        Err(e) => (
            StatusCode::BAD_REQUEST,
            format!("protobuf decode error: {e}"),
        ),
    }
}

// ---------------------------------------------------------------------------
// Transport-agnostic storage
// ---------------------------------------------------------------------------

pub async fn store_traces(pool: &PgPool, req: ExportTraceServiceRequest) -> u64 {
    let mut count = 0;
    for rs in &req.resource_spans {
        let rattrs = resource_attrs(rs.resource.as_ref());
        let service = service_name(rattrs);
        for ss in &rs.scope_spans {
            for span in &ss.spans {
                match insert_span(pool, service.as_deref(), rattrs, span).await {
                    Ok(()) => count += 1,
                    Err(e) => tracing::warn!("insert span failed: {e}"),
                }
            }
        }
    }
    count
}

pub async fn store_logs(pool: &PgPool, req: ExportLogsServiceRequest) -> u64 {
    let mut count = 0;
    for rl in &req.resource_logs {
        // Keep resource attributes (k8s.pod.name / node / container, …) so logs
        // can be filtered by pod/host, not just service.
        let rattrs = resource_attrs(rl.resource.as_ref());
        let service = service_name(rattrs);
        for sl in &rl.scope_logs {
            for rec in &sl.log_records {
                match insert_log(pool, service.as_deref(), rattrs, rec).await {
                    Ok(()) => count += 1,
                    Err(e) => tracing::warn!("insert log failed: {e}"),
                }
            }
        }
    }
    count
}

pub async fn store_metrics(pool: &PgPool, req: ExportMetricsServiceRequest) -> u64 {
    let mut count = 0;
    for rm in &req.resource_metrics {
        // Resource attributes carry k8s.pod.name / node / container etc. — keep them
        // so metrics are dimensioned, not flat.
        let rattrs = resource_attrs(rm.resource.as_ref());
        let service = service_name(rattrs);
        for sm in &rm.scope_metrics {
            for m in &sm.metrics {
                count += insert_metric(pool, service.as_deref(), rattrs, m).await;
            }
        }
    }
    count
}

// ---------------------------------------------------------------------------
// Inserts
// ---------------------------------------------------------------------------

async fn insert_span(
    pool: &PgPool,
    service: Option<&str>,
    resource: &[KeyValue],
    span: &Span,
) -> anyhow::Result<()> {
    let trace_id = hex::encode(&span.trace_id);
    let span_id = hex::encode(&span.span_id);
    let parent = (!span.parent_span_id.is_empty()).then(|| hex::encode(&span.parent_span_id));
    let start = ts(span.start_time_unix_nano);
    let end = ts(span.end_time_unix_nano);
    let duration_ms = span
        .end_time_unix_nano
        .saturating_sub(span.start_time_unix_nano) as f64
        / 1_000_000.0;
    let (status_code, status_message) = match &span.status {
        Some(s) => (
            Some(s.code),
            (!s.message.is_empty()).then(|| s.message.clone()),
        ),
        None => (None, None),
    };
    let attrs = merged_attrs(resource, &span.attributes);

    sqlx::query(
        "INSERT INTO spans (trace_id, span_id, parent_span_id, service, name, kind,
             start_time, end_time, duration_ms, status_code, status_message, attributes)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12)
         ON CONFLICT (trace_id, span_id) DO NOTHING",
    )
    .bind(trace_id)
    .bind(span_id)
    .bind(parent)
    .bind(service)
    .bind(&span.name)
    .bind(span.kind)
    .bind(start)
    .bind(end)
    .bind(duration_ms)
    .bind(status_code)
    .bind(status_message)
    .bind(attrs)
    .execute(pool)
    .await?;
    Ok(())
}

async fn insert_log(
    pool: &PgPool,
    service: Option<&str>,
    resource: &[KeyValue],
    rec: &LogRecord,
) -> anyhow::Result<()> {
    let nanos = if rec.time_unix_nano != 0 {
        rec.time_unix_nano
    } else {
        rec.observed_time_unix_nano
    };
    let time = ts(nanos);
    let trace_id = (!rec.trace_id.is_empty()).then(|| hex::encode(&rec.trace_id));
    let span_id = (!rec.span_id.is_empty()).then(|| hex::encode(&rec.span_id));
    let body = rec.body.as_ref().map(any_value_to_text);
    let attrs = merged_attrs(resource, &rec.attributes);

    sqlx::query(
        "INSERT INTO logs (time, trace_id, span_id, service, severity_number, severity_text, body, attributes)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8)",
    )
    .bind(time)
    .bind(trace_id)
    .bind(span_id)
    .bind(service)
    .bind(rec.severity_number)
    .bind(&rec.severity_text)
    .bind(body)
    .bind(attrs)
    .execute(pool)
    .await?;
    Ok(())
}

async fn insert_metric(
    pool: &PgPool,
    service: Option<&str>,
    resource: &[KeyValue],
    m: &Metric,
) -> u64 {
    let unit = (!m.unit.is_empty()).then(|| m.unit.clone());
    let mut n = 0;
    match &m.data {
        Some(metric::Data::Gauge(g)) => {
            for dp in &g.data_points {
                n += insert_number(
                    pool,
                    service,
                    resource,
                    &m.name,
                    "gauge",
                    unit.as_deref(),
                    None,
                    dp,
                )
                .await as u64;
            }
        }
        Some(metric::Data::Sum(s)) => {
            // is_monotonic distinguishes a counter (rate it) from an
            // UpDownCounter (a gauge-like running value).
            for dp in &s.data_points {
                n += insert_number(
                    pool,
                    service,
                    resource,
                    &m.name,
                    "sum",
                    unit.as_deref(),
                    Some(s.is_monotonic),
                    dp,
                )
                .await as u64;
            }
        }
        Some(metric::Data::Histogram(h)) => {
            for dp in &h.data_points {
                n += insert_histogram(pool, service, resource, &m.name, unit.as_deref(), dp).await
                    as u64;
            }
        }
        // Exponential histograms and summaries aren't stored in v0.
        _ => {}
    }
    n
}

async fn insert_number(
    pool: &PgPool,
    service: Option<&str>,
    resource: &[KeyValue],
    name: &str,
    kind: &str,
    unit: Option<&str>,
    is_monotonic: Option<bool>,
    dp: &NumberDataPoint,
) -> bool {
    let value = match dp.value {
        Some(number_data_point::Value::AsDouble(d)) => d,
        Some(number_data_point::Value::AsInt(i)) => i as f64,
        None => return false,
    };
    // Aggregate-on-insert: keep the raw point (short, capped retention) AND fold
    // it into its per-series rollup bucket in one statement, so the rollup's
    // current bucket is always live — no batch sweep, and reads stay fresh.
    let res = sqlx::query(
        "WITH raw AS (
             INSERT INTO metrics (time, service, name, kind, value, unit, attributes, is_monotonic)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8)
         )
         INSERT INTO metric_series_rollups
             (bucket, name, series_key, attrs, service, kind, unit, is_monotonic,
              count, sum, min, max, avg)
         VALUES (to_timestamp(floor(extract(epoch FROM $1::timestamptz) / $9) * $9),
                 $3, md5(coalesce($2,'') || '|' || $7::text), $7, $2, $4, $6, $8,
                 1, $5, $5, $5, $5)
         ON CONFLICT (name, series_key, bucket) DO UPDATE SET
             count = metric_series_rollups.count + 1,
             sum   = metric_series_rollups.sum + EXCLUDED.sum,
             min   = least(metric_series_rollups.min, EXCLUDED.min),
             max   = greatest(metric_series_rollups.max, EXCLUDED.max),
             avg   = (metric_series_rollups.sum + EXCLUDED.sum)
                     / (metric_series_rollups.count + 1),
             unit  = EXCLUDED.unit,
             is_monotonic = EXCLUDED.is_monotonic",
    )
    .bind(ts(dp.time_unix_nano))
    .bind(service)
    .bind(name)
    .bind(kind)
    .bind(value)
    .bind(unit)
    .bind(merged_attrs(resource, &dp.attributes))
    .bind(is_monotonic)
    .bind(rollup_width())
    .execute(pool)
    .await;
    match res {
        Ok(_) => true,
        Err(e) => {
            tracing::warn!("insert metric failed: {e}");
            false
        }
    }
}

async fn insert_histogram(
    pool: &PgPool,
    service: Option<&str>,
    resource: &[KeyValue],
    name: &str,
    unit: Option<&str>,
    dp: &HistogramDataPoint,
) -> bool {
    // bucket_counts has one more entry than explicit_bounds (the +Inf bucket).
    let bounds: Vec<f64> = dp.explicit_bounds.clone();
    let counts: Vec<i64> = dp.bucket_counts.iter().map(|&c| c as i64).collect();
    // Raw point + additive fold into the per-series histogram rollup (counts
    // summed element-wise via array_add) in one statement — see insert_number.
    let res = sqlx::query(
        "WITH raw AS (
             INSERT INTO metrics
                 (time, service, name, kind, value, count, unit, attributes, bucket_bounds, bucket_counts)
             VALUES ($1,$2,$3,'histogram',$4,$5,$6,$7,$8,$9)
         )
         INSERT INTO metric_series_rollups
             (bucket, name, series_key, attrs, service, kind, unit,
              count, sum, min, max, avg, bucket_bounds, bucket_counts)
         VALUES (to_timestamp(floor(extract(epoch FROM $1::timestamptz) / $10) * $10),
                 $3, md5(coalesce($2,'') || '|' || $7::text), $7, $2, 'histogram', $6,
                 $5, $4, $4, $4, $4, $8, $9)
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
    .bind(ts(dp.time_unix_nano))
    .bind(service)
    .bind(name)
    .bind(dp.sum)
    .bind(dp.count as i64)
    .bind(unit)
    .bind(merged_attrs(resource, &dp.attributes))
    .bind(&bounds)
    .bind(&counts)
    .bind(rollup_width())
    .execute(pool)
    .await;
    match res {
        Ok(_) => true,
        Err(e) => {
            tracing::warn!("insert histogram failed: {e}");
            false
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn ts(nanos: u64) -> DateTime<Utc> {
    DateTime::from_timestamp_nanos(nanos as i64)
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

fn attrs_to_json(attrs: &[KeyValue]) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    for kv in attrs {
        if let Some(v) = &kv.value {
            map.insert(kv.key.clone(), any_value_to_json(v));
        }
    }
    serde_json::Value::Object(map)
}

/// Resource attributes (k8s.pod.name / node / container, …) overlaid with a data
/// point's own attributes — so stored metrics keep their dimensions. The point's
/// attributes win on a key collision.
fn merged_attrs(resource: &[KeyValue], point: &[KeyValue]) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    for kv in resource.iter().chain(point.iter()) {
        if let Some(v) = &kv.value {
            map.insert(kv.key.clone(), any_value_to_json(v));
        }
    }
    serde_json::Value::Object(map)
}

fn any_value_to_json(v: &AnyValue) -> serde_json::Value {
    use any_value::Value as V;
    match &v.value {
        Some(V::StringValue(s)) => json!(s),
        Some(V::BoolValue(b)) => json!(b),
        Some(V::IntValue(i)) => json!(i),
        Some(V::DoubleValue(d)) => json!(d),
        Some(V::BytesValue(b)) => json!(hex::encode(b)),
        Some(V::ArrayValue(a)) => {
            serde_json::Value::Array(a.values.iter().map(any_value_to_json).collect())
        }
        Some(V::KvlistValue(kv)) => attrs_to_json(&kv.values),
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
    fn service_name_extraction() {
        let attrs = vec![
            KeyValue {
                key: "host".into(),
                value: Some(sval("box")),
            },
            KeyValue {
                key: "service.name".into(),
                value: Some(sval("checkout")),
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
            },
            KeyValue {
                key: "k8s.deployment.name".into(),
                value: Some(sval("checkout")),
            },
        ];
        assert_eq!(service_name(&attrs).as_deref(), Some("checkout"));

        // unknown_service with no k8s hints and no configured default → None.
        let bare = vec![KeyValue {
            key: "service.name".into(),
            value: Some(sval("unknown_service")),
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
        }];
        assert_eq!(service_name(&attrs), None);
    }

    #[test]
    fn attrs_to_json_covers_all_value_kinds() {
        let attrs = vec![
            KeyValue {
                key: "s".into(),
                value: Some(sval("x")),
            },
            KeyValue {
                key: "b".into(),
                value: Some(AnyValue {
                    value: Some(V::BoolValue(true)),
                }),
            },
            KeyValue {
                key: "i".into(),
                value: Some(AnyValue {
                    value: Some(V::IntValue(42)),
                }),
            },
            KeyValue {
                key: "d".into(),
                value: Some(AnyValue {
                    value: Some(V::DoubleValue(1.5)),
                }),
            },
            KeyValue {
                key: "by".into(),
                value: Some(AnyValue {
                    value: Some(V::BytesValue(vec![0xde, 0xad])),
                }),
            },
            KeyValue {
                key: "arr".into(),
                value: Some(AnyValue {
                    value: Some(V::ArrayValue(ArrayValue {
                        values: vec![sval("a"), sval("b")],
                    })),
                }),
            },
            KeyValue {
                key: "kv".into(),
                value: Some(AnyValue {
                    value: Some(V::KvlistValue(KeyValueList {
                        values: vec![KeyValue {
                            key: "nested".into(),
                            value: Some(sval("y")),
                        }],
                    })),
                }),
            },
        ];
        let json = attrs_to_json(&attrs);
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
}
