//! OTLP decode + storage. `store_*` are transport-agnostic (HTTP handlers and the
//! gRPC services both call them); the `ingest_*` fns are the HTTP/protobuf entrypoints.

use axum::{body::Bytes, extract::State, http::StatusCode, response::IntoResponse};
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

// ---------------------------------------------------------------------------
// HTTP entrypoints
// ---------------------------------------------------------------------------

pub async fn ingest_traces(State(pool): State<PgPool>, body: Bytes) -> impl IntoResponse {
    match ExportTraceServiceRequest::decode(body) {
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

pub async fn ingest_logs(State(pool): State<PgPool>, body: Bytes) -> impl IntoResponse {
    match ExportLogsServiceRequest::decode(body) {
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

pub async fn ingest_metrics(State(pool): State<PgPool>, body: Bytes) -> impl IntoResponse {
    match ExportMetricsServiceRequest::decode(body) {
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
        let service = service_name(resource_attrs(rs.resource.as_ref()));
        for ss in &rs.scope_spans {
            for span in &ss.spans {
                match insert_span(pool, service.as_deref(), span).await {
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
        let service = service_name(resource_attrs(rl.resource.as_ref()));
        for sl in &rl.scope_logs {
            for rec in &sl.log_records {
                match insert_log(pool, service.as_deref(), rec).await {
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
        let service = service_name(resource_attrs(rm.resource.as_ref()));
        for sm in &rm.scope_metrics {
            for m in &sm.metrics {
                count += insert_metric(pool, service.as_deref(), m).await;
            }
        }
    }
    count
}

// ---------------------------------------------------------------------------
// Inserts
// ---------------------------------------------------------------------------

async fn insert_span(pool: &PgPool, service: Option<&str>, span: &Span) -> anyhow::Result<()> {
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
    let attrs = attrs_to_json(&span.attributes);

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

async fn insert_log(pool: &PgPool, service: Option<&str>, rec: &LogRecord) -> anyhow::Result<()> {
    let nanos = if rec.time_unix_nano != 0 {
        rec.time_unix_nano
    } else {
        rec.observed_time_unix_nano
    };
    let time = ts(nanos);
    let trace_id = (!rec.trace_id.is_empty()).then(|| hex::encode(&rec.trace_id));
    let span_id = (!rec.span_id.is_empty()).then(|| hex::encode(&rec.span_id));
    let body = rec.body.as_ref().map(any_value_to_text);
    let attrs = attrs_to_json(&rec.attributes);

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

async fn insert_metric(pool: &PgPool, service: Option<&str>, m: &Metric) -> u64 {
    let unit = (!m.unit.is_empty()).then(|| m.unit.clone());
    let mut n = 0;
    match &m.data {
        Some(metric::Data::Gauge(g)) => {
            for dp in &g.data_points {
                n += insert_number(pool, service, &m.name, "gauge", unit.as_deref(), dp).await
                    as u64;
            }
        }
        Some(metric::Data::Sum(s)) => {
            for dp in &s.data_points {
                n += insert_number(pool, service, &m.name, "sum", unit.as_deref(), dp).await as u64;
            }
        }
        Some(metric::Data::Histogram(h)) => {
            for dp in &h.data_points {
                n += insert_histogram(pool, service, &m.name, unit.as_deref(), dp).await as u64;
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
    name: &str,
    kind: &str,
    unit: Option<&str>,
    dp: &NumberDataPoint,
) -> bool {
    let value = match dp.value {
        Some(number_data_point::Value::AsDouble(d)) => d,
        Some(number_data_point::Value::AsInt(i)) => i as f64,
        None => return false,
    };
    let res = sqlx::query(
        "INSERT INTO metrics (time, service, name, kind, value, unit, attributes)
         VALUES ($1,$2,$3,$4,$5,$6,$7)",
    )
    .bind(ts(dp.time_unix_nano))
    .bind(service)
    .bind(name)
    .bind(kind)
    .bind(value)
    .bind(unit)
    .bind(attrs_to_json(&dp.attributes))
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
    name: &str,
    unit: Option<&str>,
    dp: &HistogramDataPoint,
) -> bool {
    let res = sqlx::query(
        "INSERT INTO metrics (time, service, name, kind, value, count, unit, attributes)
         VALUES ($1,$2,$3,'histogram',$4,$5,$6,$7)",
    )
    .bind(ts(dp.time_unix_nano))
    .bind(service)
    .bind(name)
    .bind(dp.sum)
    .bind(dp.count as i64)
    .bind(unit)
    .bind(attrs_to_json(&dp.attributes))
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

fn resource_attrs(
    resource: Option<&opentelemetry_proto::tonic::resource::v1::Resource>,
) -> &[KeyValue] {
    resource.map(|r| r.attributes.as_slice()).unwrap_or(&[])
}

fn service_name(attrs: &[KeyValue]) -> Option<String> {
    attrs
        .iter()
        .find(|kv| kv.key == "service.name")
        .and_then(|kv| kv.value.as_ref())
        .and_then(|v| match &v.value {
            Some(any_value::Value::StringValue(s)) => Some(s.clone()),
            _ => None,
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
