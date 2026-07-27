//! End-to-end ingest → query test. Requires a Postgres reachable via DATABASE_URL
//! (CI provides one as a service container); skips cleanly if it's unset.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use opentelemetry_proto::tonic::{
    collector::logs::v1::ExportLogsServiceRequest,
    collector::metrics::v1::ExportMetricsServiceRequest,
    collector::trace::v1::ExportTraceServiceRequest,
    common::v1::{any_value, AnyValue, KeyValue},
    logs::v1::{LogRecord, ResourceLogs, ScopeLogs},
    metrics::v1::{
        exemplar, metric, number_data_point, Exemplar, Gauge, Histogram, HistogramDataPoint,
        Metric, NumberDataPoint, ResourceMetrics, ScopeMetrics, Sum,
    },
    resource::v1::Resource,
    trace::v1::{ResourceSpans, ScopeSpans, Span},
};
use prost::Message;
use serde_json::json;
use serial_test::serial;
use std::time::{SystemTime, UNIX_EPOCH};
use tower::ServiceExt;
use tracing_subscriber::layer::SubscriberExt;
use watcher_server::{
    access_jwt, alerts, app, app_with_access, app_with_auth, db, mcp_auth, selflog, selfmon,
    selftrace,
};

fn now_nanos() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64
}

/// Build an OTLP metrics request for a single recent gauge point.
fn gauge_request(name: &str, value: f64, nanos: u64) -> ExportMetricsServiceRequest {
    ExportMetricsServiceRequest {
        resource_metrics: vec![ResourceMetrics {
            resource: Some(Resource {
                attributes: vec![kv("service.name", "api")],
                ..Default::default()
            }),
            scope_metrics: vec![ScopeMetrics {
                metrics: vec![Metric {
                    name: name.to_string(),
                    unit: "1".to_string(),
                    data: Some(metric::Data::Gauge(Gauge {
                        data_points: vec![NumberDataPoint {
                            time_unix_nano: nanos,
                            value: Some(number_data_point::Value::AsDouble(value)),
                            ..Default::default()
                        }],
                    })),
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        }],
    }
}

/// One metrics request carrying several gauge points (each tagged `pod=`), so the
/// batched ingest path sees multiple series — and a repeated pod — in one request.
fn gauge_points_request(
    name: &str,
    points: &[(&str, f64)],
    nanos: u64,
) -> ExportMetricsServiceRequest {
    ExportMetricsServiceRequest {
        resource_metrics: vec![ResourceMetrics {
            resource: Some(Resource {
                attributes: vec![kv("service.name", "api")],
                ..Default::default()
            }),
            scope_metrics: vec![ScopeMetrics {
                metrics: vec![Metric {
                    name: name.to_string(),
                    unit: "1".to_string(),
                    data: Some(metric::Data::Gauge(Gauge {
                        data_points: points
                            .iter()
                            .map(|(pod, v)| NumberDataPoint {
                                time_unix_nano: nanos,
                                value: Some(number_data_point::Value::AsDouble(*v)),
                                attributes: vec![kv("pod", pod)],
                                ..Default::default()
                            })
                            .collect(),
                    })),
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        }],
    }
}

/// Unix-nanos timestamp `secs` in the past, for ingesting points into a past bucket.
fn nanos_ago(secs: u64) -> u64 {
    now_nanos() - secs * 1_000_000_000
}

/// Wrap one metric (already built) in a single-service OTLP request.
fn metric_request(name: &str, service: &str, data: metric::Data) -> ExportMetricsServiceRequest {
    ExportMetricsServiceRequest {
        resource_metrics: vec![ResourceMetrics {
            resource: Some(Resource {
                attributes: vec![kv("service.name", service)],
                ..Default::default()
            }),
            scope_metrics: vec![ScopeMetrics {
                metrics: vec![Metric {
                    name: name.to_string(),
                    unit: "1".to_string(),
                    data: Some(data),
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        }],
    }
}

/// One gauge (`monotonic == None`) or sum/counter point, optionally `pod`-tagged.
fn one_number(
    name: &str,
    monotonic: Option<bool>,
    service: &str,
    pod: Option<&str>,
    value: f64,
    nanos: u64,
) -> ExportMetricsServiceRequest {
    let dp = NumberDataPoint {
        time_unix_nano: nanos,
        value: Some(number_data_point::Value::AsDouble(value)),
        attributes: pod.map(|p| vec![kv("pod", p)]).unwrap_or_default(),
        ..Default::default()
    };
    let data = match monotonic {
        None => metric::Data::Gauge(Gauge {
            data_points: vec![dp],
        }),
        Some(is_monotonic) => metric::Data::Sum(Sum {
            data_points: vec![dp],
            is_monotonic,
            aggregation_temporality: 2, // cumulative
        }),
    };
    metric_request(name, service, data)
}

/// One gauge point carrying a single OTLP exemplar (JEF-433), so ingest's
/// `first_exemplar` decode path has something to pick up. `trace_id`/`span_id`
/// are raw bytes — hex-encode the return value to compare against the API.
fn gauge_with_exemplar(
    name: &str,
    service: &str,
    value: f64,
    trace_id: Vec<u8>,
    span_id: Vec<u8>,
    nanos: u64,
) -> ExportMetricsServiceRequest {
    let dp = NumberDataPoint {
        time_unix_nano: nanos,
        value: Some(number_data_point::Value::AsDouble(value)),
        exemplars: vec![Exemplar {
            time_unix_nano: nanos,
            trace_id,
            span_id,
            value: Some(exemplar::Value::AsDouble(value)),
            ..Default::default()
        }],
        ..Default::default()
    };
    metric_request(
        name,
        service,
        metric::Data::Gauge(Gauge {
            data_points: vec![dp],
        }),
    )
}

/// One histogram point with the given bounds/counts, optionally `pod`-tagged.
#[allow(clippy::too_many_arguments)] // test helper — a struct would only add noise
fn one_histogram(
    name: &str,
    service: &str,
    pod: Option<&str>,
    bounds: Vec<f64>,
    counts: Vec<u64>,
    count: u64,
    sum: f64,
    nanos: u64,
) -> ExportMetricsServiceRequest {
    let dp = HistogramDataPoint {
        time_unix_nano: nanos,
        count,
        sum: Some(sum),
        explicit_bounds: bounds,
        bucket_counts: counts,
        attributes: pod.map(|p| vec![kv("pod", p)]).unwrap_or_default(),
        ..Default::default()
    };
    metric_request(
        name,
        service,
        metric::Data::Histogram(Histogram {
            aggregation_temporality: 2,
            data_points: vec![dp],
        }),
    )
}

/// Ingest a metrics request through the real OTLP path (so aggregate-on-insert
/// populates the per-series rollup), asserting it's accepted.
async fn ingest(router: &axum::Router, req: ExportMetricsServiceRequest) {
    assert_eq!(
        post_proto(router, "/v1/metrics", req.encode_to_vec()).await,
        StatusCode::OK
    );
}

async fn post_proto(router: &axum::Router, uri: &str, body: Vec<u8>) -> StatusCode {
    router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header("content-type", "application/x-protobuf")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap()
        .status()
}

async fn get_json(router: &axum::Router, uri: &str) -> (StatusCode, serde_json::Value) {
    let resp = router
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json = if bytes.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
    };
    (status, json)
}

fn kv(key: &str, value: &str) -> KeyValue {
    KeyValue {
        key: key.to_string(),
        value: Some(AnyValue {
            value: Some(any_value::Value::StringValue(value.to_string())),
        }),
        ..Default::default()
    }
}

async fn pool_or_skip() -> Option<sqlx::PgPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let pool = db::connect(&url).await.expect("connect");
    db::migrate(&pool).await.expect("migrate");
    sqlx::query("TRUNCATE spans, logs, metrics, metric_series_rollups, alert_rules, alert_events")
        .execute(&pool)
        .await
        .expect("truncate");
    Some(pool)
}

#[tokio::test]
#[serial]
async fn ingest_and_query_a_trace() {
    let Some(pool) = pool_or_skip().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let router = app(pool);

    let start = nanos_ago(5);
    let req = ExportTraceServiceRequest {
        resource_spans: vec![ResourceSpans {
            resource: Some(Resource {
                attributes: vec![kv("service.name", "checkout")],
                ..Default::default()
            }),
            scope_spans: vec![ScopeSpans {
                spans: vec![Span {
                    trace_id: vec![1u8; 16],
                    span_id: vec![2u8; 8],
                    name: "GET /checkout".to_string(),
                    // recent (within the list's default window), +50ms duration
                    start_time_unix_nano: start,
                    end_time_unix_nano: start + 50_000_000,
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        }],
    };

    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/traces")
                .header("content-type", "application/x-protobuf")
                .body(Body::from(req.encode_to_vec()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let resp = router
        .oneshot(
            Request::builder()
                .uri("/api/traces")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let traces: serde_json::Value = serde_json::from_slice(&bytes).unwrap();

    let arr = traces.as_array().expect("array");
    assert_eq!(arr.len(), 1, "expected exactly one trace");
    assert_eq!(arr[0]["service"], "checkout");
    assert_eq!(arr[0]["root_name"], "GET /checkout");
    assert_eq!(arr[0]["span_count"], 1);
    assert_eq!(arr[0]["duration_ms"], 50.0);
}

#[tokio::test]
#[serial]
async fn ingest_and_query_a_log() {
    let Some(pool) = pool_or_skip().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let router = app(pool);

    let req = ExportLogsServiceRequest {
        resource_logs: vec![ResourceLogs {
            resource: Some(Resource {
                attributes: vec![kv("service.name", "api")],
                ..Default::default()
            }),
            scope_logs: vec![ScopeLogs {
                log_records: vec![LogRecord {
                    // Real "now" rather than a fixed epoch value: /api/logs floors
                    // to a recent default window (JEF-546), so a fake 1970
                    // timestamp would fall outside it and never come back.
                    time_unix_nano: now_nanos(),
                    severity_number: 9, // INFO
                    severity_text: "INFO".to_string(),
                    body: Some(AnyValue {
                        value: Some(any_value::Value::StringValue("hello world".to_string())),
                    }),
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        }],
    };

    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/logs")
                .header("content-type", "application/x-protobuf")
                .body(Body::from(req.encode_to_vec()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let resp = router
        .oneshot(
            Request::builder()
                .uri("/api/logs?q=hello")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let logs: serde_json::Value = serde_json::from_slice(&bytes).unwrap();

    let arr = logs.as_array().expect("array");
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["service"], "api");
    assert_eq!(arr[0]["body"], "hello world");
    assert_eq!(arr[0]["severity_text"], "INFO");
}

#[tokio::test]
#[serial]
async fn ingest_a_log_batch_persists_every_row_in_one_call() {
    // JEF-495: store_logs batches a whole request into chunked INSERTs. A single
    // multi-record request must land every row (identical column values) and not
    // touch DROP_INSERT.
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let router = app(pool.clone());

    const N: usize = 250;
    let drops_before = selfmon::DROP_INSERT.load(std::sync::atomic::Ordering::Relaxed);
    // Real "now" rather than a fixed epoch value: /api/logs floors to a recent
    // default window (JEF-546), so fake 1970 timestamps would fall outside it
    // and never come back via the `/api/logs?service=batcher` check below.
    let base_nanos = now_nanos();
    let records: Vec<LogRecord> = (0..N)
        .map(|i| LogRecord {
            time_unix_nano: base_nanos + i as u64,
            severity_number: 9,
            severity_text: "INFO".to_string(),
            trace_id: vec![(i % 251) as u8 + 1; 16],
            body: Some(AnyValue {
                value: Some(any_value::Value::StringValue(format!("line {i}"))),
            }),
            ..Default::default()
        })
        .collect();
    let req = ExportLogsServiceRequest {
        resource_logs: vec![ResourceLogs {
            resource: Some(Resource {
                attributes: vec![kv("service.name", "batcher")],
                ..Default::default()
            }),
            scope_logs: vec![ScopeLogs {
                log_records: records,
                ..Default::default()
            }],
            ..Default::default()
        }],
    };

    assert_eq!(
        post_proto(&router, "/v1/logs", req.encode_to_vec()).await,
        StatusCode::OK
    );

    // Every row persisted in the one call — the batched path, not O(N) inserts.
    assert_eq!(count(&pool, "logs").await, N as i64);
    assert_eq!(
        selfmon::DROP_INSERT.load(std::sync::atomic::Ordering::Relaxed),
        drops_before,
        "a clean batch drops nothing"
    );

    // Column values survive the UNNEST round-trip intact.
    let (status, logs) = get_json(&router, "/api/logs?service=batcher&limit=1000").await;
    assert_eq!(status, StatusCode::OK);
    let arr = logs.as_array().unwrap();
    assert_eq!(arr.len(), N);
    let first = arr
        .iter()
        .find(|l| l["body"] == "line 0")
        .expect("line 0 present");
    assert_eq!(first["service"], "batcher");
    assert_eq!(first["severity_text"], "INFO");
    assert!(first["trace_id"].is_string(), "trace_id preserved");
}

#[tokio::test]
#[serial]
async fn ingest_a_span_batch_persists_and_dedupes_in_one_call() {
    // JEF-495: store_traces batches too. Distinct spans all land; an intra-batch
    // duplicate (same trace_id/span_id) is collapsed by ON CONFLICT DO NOTHING,
    // exactly as the old per-row insert did.
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let router = app(pool.clone());

    const N: usize = 100;
    let start = nanos_ago(5);
    let mut spans: Vec<Span> = (0..N)
        .map(|i| Span {
            trace_id: vec![7u8; 16],
            span_id: (i as u64 + 1).to_be_bytes().to_vec(),
            name: format!("op {i}"),
            start_time_unix_nano: start,
            end_time_unix_nano: start + 1_000_000,
            ..Default::default()
        })
        .collect();
    // A duplicate of span #1 in the same batch — must not create a second row.
    spans.push(Span {
        trace_id: vec![7u8; 16],
        span_id: 1u64.to_be_bytes().to_vec(),
        name: "op 0 dup".to_string(),
        start_time_unix_nano: start,
        end_time_unix_nano: start + 1_000_000,
        ..Default::default()
    });

    let req = ExportTraceServiceRequest {
        resource_spans: vec![ResourceSpans {
            resource: Some(Resource {
                attributes: vec![kv("service.name", "spanbatch")],
                ..Default::default()
            }),
            scope_spans: vec![ScopeSpans {
                spans,
                ..Default::default()
            }],
            ..Default::default()
        }],
    };

    assert_eq!(
        post_proto(&router, "/v1/traces", req.encode_to_vec()).await,
        StatusCode::OK
    );

    // N distinct spans landed; the duplicate did not add a row.
    let stored: i64 = sqlx::query_scalar("SELECT count(*) FROM spans WHERE service = 'spanbatch'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        stored, N as i64,
        "distinct spans persisted, duplicate deduped"
    );
}

#[tokio::test]
#[serial]
async fn ingest_and_query_a_metric() {
    let Some(pool) = pool_or_skip().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let router = app(pool);

    let req = ExportMetricsServiceRequest {
        resource_metrics: vec![ResourceMetrics {
            resource: Some(Resource {
                attributes: vec![kv("service.name", "api")],
                ..Default::default()
            }),
            scope_metrics: vec![ScopeMetrics {
                metrics: vec![Metric {
                    name: "http.requests".to_string(),
                    unit: "1".to_string(),
                    data: Some(metric::Data::Gauge(Gauge {
                        data_points: vec![NumberDataPoint {
                            time_unix_nano: 1_000_000_000,
                            value: Some(number_data_point::Value::AsDouble(42.0)),
                            ..Default::default()
                        }],
                    })),
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        }],
    };

    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/metrics")
                .header("content-type", "application/x-protobuf")
                .body(Body::from(req.encode_to_vec()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let resp = router
        .oneshot(
            Request::builder()
                .uri("/api/metrics")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let metrics: serde_json::Value = serde_json::from_slice(&bytes).unwrap();

    let arr = metrics.as_array().expect("array");
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["name"], "http.requests");
    assert_eq!(arr[0]["kind"], "gauge");
    assert_eq!(arr[0]["last_value"], 42.0);
}

#[tokio::test]
#[serial]
async fn metric_series_returns_recent_points() {
    let Some(pool) = pool_or_skip().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let router = app(pool);

    let now = now_nanos();
    let req = gauge_request("cpu.load", 0.7, now);
    assert_eq!(
        post_proto(&router, "/v1/metrics", req.encode_to_vec()).await,
        StatusCode::OK
    );

    // Aggregate-on-insert wrote the rollup, so the series reads it back.
    let (status, series) = get_json(&router, "/api/metrics/series?name=cpu.load&hours=1").await;
    assert_eq!(status, StatusCode::OK);
    let arr = series.as_array().expect("array");
    assert_eq!(arr.len(), 1, "expected one bucketed point");
    assert!(arr[0]["t"].is_string());
    assert_eq!(arr[0]["v"], 0.7);

    // A metric that doesn't exist yields an empty series, not an error.
    let (status, empty) = get_json(&router, "/api/metrics/series?name=nope").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(empty.as_array().unwrap().len(), 0);
}

#[tokio::test]
#[serial]
async fn metric_series_honors_absolute_from_to_window() {
    let Some(pool) = pool_or_skip().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let router = app(pool);

    // Two points an hour apart: one 3h ago (outside a 1h-wide absolute window
    // anchored 2h ago), one 90m ago (inside it).
    ingest(&router, gauge_request("cpu.load", 0.1, nanos_ago(3 * 3600))).await;
    ingest(&router, gauge_request("cpu.load", 0.9, nanos_ago(5400))).await;

    // RFC3339 with a `Z` suffix (no `+`) — a literal `+` in a query string is
    // form-decoded as a space, corrupting a `+00:00` offset (see
    // time_window_filters_traces_and_logs for the same pitfall).
    let now = chrono::Utc::now();
    let from =
        (now - chrono::Duration::hours(2)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let to = (now - chrono::Duration::hours(1)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let (status, series) = get_json(
        &router,
        &format!("/api/metrics/series?name=cpu.load&from={from}&to={to}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let arr = series.as_array().expect("array");
    assert_eq!(arr.len(), 1, "only the point inside the absolute window");
    assert_eq!(arr[0]["v"], 0.9);

    // Same window via facet and histogram — both must render the exact absolute
    // range too, not just `series` (JEF-433).
    ingest(
        &router,
        one_number("reqs", None, "api", None, 0.9, nanos_ago(5400)),
    )
    .await;
    ingest(
        &router,
        one_number("reqs", None, "api", None, 0.1, nanos_ago(3 * 3600)),
    )
    .await;
    let (status, facet) = get_json(
        &router,
        &format!("/api/metrics/facet?name=reqs&from={from}&to={to}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let series = facet["series"].as_array().unwrap();
    assert_eq!(series.len(), 1);
    let points = series[0]["points"].as_array().unwrap();
    assert_eq!(points.len(), 1, "only the in-window bucket");
    assert_eq!(points[0]["v"], 0.9);

    ingest(
        &router,
        one_histogram(
            "lat2",
            "api",
            None,
            vec![10.0, 20.0],
            vec![0, 5, 0],
            5,
            75.0,
            nanos_ago(5400),
        ),
    )
    .await;
    ingest(
        &router,
        one_histogram(
            "lat2",
            "api",
            None,
            vec![10.0, 20.0],
            vec![5, 0, 0],
            5,
            50.0,
            nanos_ago(3 * 3600),
        ),
    )
    .await;
    let (status, hist) = get_json(
        &router,
        &format!("/api/metrics/histogram?name=lat2&from={from}&to={to}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let buckets = hist["buckets"].as_array().unwrap();
    assert_eq!(buckets.len(), 1, "only the in-window bucket");
    assert_eq!(buckets[0]["counts"], serde_json::json!([0, 5, 0]));
}

#[tokio::test]
#[serial]
async fn metric_exemplar_persists_trace_id_and_surfaces_in_exemplars_endpoint() {
    let Some(pool) = pool_or_skip().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let router = app(pool.clone());

    let trace_id = vec![0xAB; 16];
    let span_id = vec![0xCD; 8];
    let now = now_nanos();
    ingest(
        &router,
        gauge_with_exemplar(
            "lat.p99",
            "api",
            42.0,
            trace_id.clone(),
            span_id.clone(),
            now,
        ),
    )
    .await;

    // The raw row keeps the exemplar's trace/span id, hex-encoded.
    let (raw_trace, raw_span): (Option<String>, Option<String>) = sqlx::query_as(
        "SELECT exemplar_trace_id, exemplar_span_id FROM metrics WHERE name = 'lat.p99'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(raw_trace.as_deref(), Some(hex::encode(&trace_id).as_str()));
    assert_eq!(raw_span.as_deref(), Some(hex::encode(&span_id).as_str()));

    // The exemplars endpoint surfaces it so a chart can deep-link to the trace.
    let (status, exemplars) =
        get_json(&router, "/api/metrics/exemplars?name=lat.p99&hours=1").await;
    assert_eq!(status, StatusCode::OK);
    let arr = exemplars.as_array().expect("array");
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["trace_id"], hex::encode(&trace_id));
    assert_eq!(arr[0]["span_id"], hex::encode(&span_id));
    assert_eq!(arr[0]["v"], 42.0);

    // A metric ingested WITHOUT an exemplar is completely unaffected: no row picks
    // up a stray trace id, and it doesn't show up in the exemplars endpoint.
    ingest(&router, gauge_request("plain.metric", 1.0, now_nanos())).await;
    let (plain_trace,): (Option<String>,) =
        sqlx::query_as("SELECT exemplar_trace_id FROM metrics WHERE name = 'plain.metric'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(plain_trace, None);
    let (status, empty) =
        get_json(&router, "/api/metrics/exemplars?name=plain.metric&hours=1").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(empty.as_array().unwrap().len(), 0);
}

#[tokio::test]
#[serial]
async fn alert_rule_fires_and_reconciles() {
    let Some(pool) = pool_or_skip().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let router = app(pool.clone());

    // Declare a rule: fire when avg(cpu.load) over the last hour exceeds 0.5.
    apply_rules(
        &pool,
        &[serde_json::json!({
            "name": "cpu hot",
            "metric": "cpu.load",
            "comparator": "gt",
            "threshold": 0.5,
            "agg": "avg",
            "window_secs": 3600,
        })],
    )
    .await;

    // Ingest a breaching point, then evaluate: the rule should fire.
    let req = gauge_request("cpu.load", 0.9, now_nanos());
    assert_eq!(
        post_proto(&router, "/v1/metrics", req.encode_to_vec()).await,
        StatusCode::OK
    );
    alerts::evaluate_once(&pool, None).await.expect("evaluate");

    let (status, rules) = get_json(&router, "/api/alerts").await;
    assert_eq!(status, StatusCode::OK);
    let rules = rules.as_array().expect("array");
    assert_eq!(rules.len(), 1);
    assert_eq!(rules[0]["firing"], true);
    // The watched metric's kind/unit are joined in so the UI can deep-link to the
    // right chart type (gauge here — a line chart, not the histogram view).
    assert_eq!(rules[0]["kind"], "gauge");

    // The firing transition is recorded as an open event, carrying the same
    // metric metadata so its history strip can deep-link too.
    let (_, events) = get_json(&router, "/api/alerts/events").await;
    let events = events.as_array().expect("array");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["value"], 0.9);
    assert!(events[0]["resolved_at"].is_null());
    assert_eq!(events[0]["kind"], "gauge");

    // Reconciling an empty config prunes the rule (its events cascade) — the
    // declarative replacement for the old delete API.
    apply_rules(&pool, &[]).await;
    let (_, rules) = get_json(&router, "/api/alerts").await;
    assert_eq!(rules.as_array().unwrap().len(), 0);
    assert_eq!(count(&pool, "alert_events").await, 0);
}

#[tokio::test]
#[serial]
async fn ui_fallback_does_not_shadow_api() {
    let Some(pool) = pool_or_skip().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let router = app(pool);

    // The query API must win over the SPA fallback — this is the regression that
    // caused JSON.parse errors when /api was served HTML.
    let (status, body) = get_json(&router, "/api/traces").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.is_array(), "/api/traces must return JSON, not the SPA");

    // An unknown (client-route) path is handled by the UI fallback, never a 500.
    let resp = router
        .oneshot(
            Request::builder()
                .uri("/some/spa/route")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_ne!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_ne!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
    // When a real UI is embedded, the SPA shell must be revalidated, or a
    // CDN/browser pins stale JS after a deploy. (The server CI job builds without
    // ui/dist, so the fallback is the "not built" 404 — nothing to assert there.)
    if resp.status() == StatusCode::OK {
        assert_eq!(
            resp.headers()
                .get("cache-control")
                .and_then(|v| v.to_str().ok()),
            Some("no-cache"),
            "SPA shell must be served no-cache"
        );
    }
}

// --- Self-monitoring + deep /healthz (JEF-425) -----------------------------

#[tokio::test]
#[serial]
async fn healthz_is_ready_when_db_up_and_retention_fresh() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let router = app(pool);

    // A reachable DB and a non-stalled retention state → ready (200) with the
    // diagnostic body the readiness probe reports.
    let resp = router
        .oneshot(
            Request::builder()
                .uri("/healthz")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["status"], "ok");
    assert_eq!(body["db"], true);
    assert_eq!(body["retention_stalled"], false);
}

#[tokio::test]
#[serial]
async fn self_telemetry_emits_watcher_ops_metrics() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    // Seed a little data so table/db-size and rollup-lag gauges have something to
    // report, then run one self-telemetry snapshot through the real store path.
    ingest(
        &app(pool.clone()),
        gauge_request("cpu.load", 0.5, now_nanos()),
    )
    .await;
    selfmon::emit_once(&pool).await.expect("emit");

    // The watcher_* series land in watcher's own metrics table, so they show up in
    // the metrics UI (same-origin /api/metrics) tagged service.name=watcher.
    let router = app(pool);
    let (status, metrics) = get_json(&router, "/api/metrics?service=watcher").await;
    assert_eq!(status, StatusCode::OK);
    let names: Vec<String> = metrics
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["name"].as_str().unwrap().to_string())
        .collect();
    for expected in [
        "watcher.db.size_bytes",
        "watcher.db.table_bytes",
        "watcher.rollup.lag_seconds",
        "watcher.retention.last_success_age_seconds",
        "watcher.ingest.metric_points_total",
    ] {
        assert!(
            names.iter().any(|n| n == expected),
            "expected {expected} in {names:?}"
        );
    }

    // db.size_bytes is a positive gauge value.
    let db_size = metrics
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["name"] == "watcher.db.size_bytes")
        .unwrap();
    assert!(db_size["last_value"].as_f64().unwrap() > 0.0);

    // table_bytes is one metric faceted per telemetry table (a table= attribute),
    // so its series_count reflects the several tables tracked.
    let tbl = metrics
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["name"] == "watcher.db.table_bytes")
        .unwrap();
    assert!(tbl["series_count"].as_i64().unwrap() >= 2);
}

// --- Self-log instrumentation (JEF-452) ------------------------------------

#[tokio::test]
#[serial]
async fn self_logs_land_in_logs_table_tagged_watcher() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    // Install the self-log layer on a thread-local subscriber (the default
    // #[tokio::test] runtime is single-threaded, so all awaited work — including the
    // drain — stays on this thread and stays under this subscriber).
    let (layer, mut rx) = selflog::channel_layer();
    let subscriber = tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::new("info,sqlx=warn"))
        .with(layer);
    let guard = tracing::subscriber::set_default(subscriber);

    tracing::info!(order_id = 7, "self log alpha");
    tracing::warn!("self log beta");
    tracing::debug!("self log filtered"); // below info → must NOT be captured

    // Drain into the DB while the subscriber is still active: storing self-logs runs
    // sqlx queries that themselves emit tracing events — those must not feed back into
    // more stored logs.
    let stored = selflog::drain_pending(&pool, &mut rx).await;
    assert_eq!(
        stored, 2,
        "info + warn captured, debug filtered by EnvFilter"
    );
    // A second drain finds nothing new: the store path generated no captured events.
    let again = selflog::drain_pending(&pool, &mut rx).await;
    assert_eq!(
        again, 0,
        "storing self-logs must not generate more self-logs"
    );
    drop(guard);

    let router = app(pool.clone());
    let (status, logs) = get_json(&router, "/api/logs?service=watcher").await;
    assert_eq!(status, StatusCode::OK);
    let arr = logs.as_array().unwrap();
    assert_eq!(
        arr.len(),
        2,
        "exactly the two emitted events, no feedback rows"
    );

    let alpha = arr
        .iter()
        .find(|l| l["body"] == "self log alpha")
        .expect("alpha row");
    assert_eq!(alpha["service"], "watcher");
    assert_eq!(alpha["severity_text"], "INFO");
    assert_eq!(alpha["severity_number"], 9);
    // Structured fields ride along as attributes.
    assert_eq!(alpha["attributes"]["order_id"], 7);
    assert_eq!(alpha["attributes"]["target"], "smoke");

    let beta = arr
        .iter()
        .find(|l| l["body"] == "self log beta")
        .expect("beta row");
    assert_eq!(beta["severity_text"], "WARN");
    assert_eq!(beta["severity_number"], 13);

    assert!(
        !arr.iter().any(|l| l["body"] == "self log filtered"),
        "debug event must be filtered out, not stored"
    );
}

#[tokio::test]
#[serial]
async fn self_logs_correlate_with_current_span() {
    use opentelemetry::trace::TracerProvider as _;
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    // With the otel layer in the stack, an event inside a span must carry that span's
    // trace/span ids so self-logs link to self-traces (the span→logs drill, JEF-429).
    let (layer, mut rx) = selflog::channel_layer();
    let provider = opentelemetry_sdk::trace::SdkTracerProvider::builder().build();
    let otel_layer = tracing_opentelemetry::layer().with_tracer(provider.tracer("test"));
    let subscriber = tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::new("info"))
        .with(otel_layer)
        .with(layer);
    let guard = tracing::subscriber::set_default(subscriber);

    tracing::info_span!("work").in_scope(|| {
        tracing::info!("inside span");
    });
    tracing::info!("outside span");

    let stored = selflog::drain_pending(&pool, &mut rx).await;
    assert_eq!(stored, 2);
    drop(guard);

    let router = app(pool.clone());
    let (_, logs) = get_json(&router, "/api/logs?service=watcher").await;
    let arr = logs.as_array().unwrap();

    let inside = arr
        .iter()
        .find(|l| l["body"] == "inside span")
        .expect("inside-span row");
    assert!(
        inside["trace_id"].is_string(),
        "in-span self-log carries a trace_id"
    );
    assert!(
        inside["span_id"].is_string(),
        "in-span self-log carries a span_id"
    );

    let outside = arr
        .iter()
        .find(|l| l["body"] == "outside span")
        .expect("outside-span row");
    assert!(
        outside["trace_id"].is_null(),
        "out-of-span self-log has no trace_id"
    );
    assert!(outside["span_id"].is_null());
}

// --- Self-trace instrumentation (JEF-462) ----------------------------------

#[tokio::test]
#[serial]
async fn self_traces_land_in_spans_and_appear_in_services() {
    use opentelemetry::trace::TracerProvider as _;
    use tracing_subscriber::Layer as _; // brings `.with_filter` into scope
    let Some(pool) = pool_or_skip().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };

    // The full self-trace path exactly as `main` wires it: an SdkTracerProvider whose
    // batch processor feeds the in-process exporter, under a tracing_opentelemetry
    // layer carrying the anti-feedback filter. A single-threaded #[tokio::test]
    // runtime keeps all awaited work (the drain) on this thread, under this subscriber.
    let (exporter, mut rx) = selftrace::channel_exporter();
    let provider = selftrace::build_provider(exporter);
    let otel_layer = tracing_opentelemetry::layer()
        .with_tracer(provider.tracer("watcher-server"))
        .with_filter(tracing_subscriber::filter::dynamic_filter_fn(|_m, _c| {
            !selflog::suppressed()
        }));
    let subscriber = tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::new("info"))
        .with(otel_layer);
    let guard = tracing::subscriber::set_default(subscriber);

    // Stand in for one of watcher's own /api request spans.
    tracing::info_span!("GET /api/traces").in_scope(|| {
        tracing::info!("handling watcher request");
    });
    // Flush the batch processor so the ended span reaches the exporter's channel. A
    // shut-down processor (the JEF-462 bug) would export nothing.
    provider.force_flush().expect("force_flush");

    // Drain into the DB while the subscriber is still active: storing self-spans runs
    // sqlx queries whose events/spans must be suppressed by the store guard, not
    // re-exported into more stored spans.
    let stored = selftrace::drain_pending(&pool, &mut rx).await;
    assert!(stored >= 1, "the watcher span was exported and stored");

    // Flush + drain again: the store path generated no captured spans, so nothing new.
    provider.force_flush().expect("force_flush");
    let again = selftrace::drain_pending(&pool, &mut rx).await;
    assert_eq!(
        again, 0,
        "storing self-spans must not generate more self-spans"
    );
    drop(guard);

    // watcher now appears in /api/services (RED per service reads the spans table),
    // tagged service=watcher.
    let router = app(pool.clone());
    let (status, services) = get_json(&router, "/api/services").await;
    assert_eq!(status, StatusCode::OK);
    let arr = services.as_array().expect("array");
    let watcher = arr
        .iter()
        .find(|s| s["service"] == "watcher")
        .expect("watcher present in /api/services");
    assert!(
        watcher["spans"].as_i64().unwrap() >= 1,
        "watcher has at least the one stored span"
    );
}

// ===========================================================================
// Helpers for the expanded suite: direct inserts (for time control) and a
// webhook-capture server.
// ===========================================================================

async fn insert_span_at(
    pool: &sqlx::PgPool,
    service: &str,
    trace: &str,
    span: &str,
    secs_ago: f64,
) {
    sqlx::query(
        "INSERT INTO spans (trace_id, span_id, service, name, start_time, end_time, duration_ms)
         VALUES ($1,$2,$3,'op', now() - make_interval(secs => $4),
                 now() - make_interval(secs => $4), 1.0)",
    )
    .bind(trace)
    .bind(span)
    .bind(service)
    .bind(secs_ago)
    .execute(pool)
    .await
    .unwrap();
}

async fn insert_log_at(pool: &sqlx::PgPool, service: &str, secs_ago: f64) {
    sqlx::query(
        "INSERT INTO logs (time, service, body) VALUES (now() - make_interval(secs => $1), $2, 'x')",
    )
    .bind(secs_ago)
    .bind(service)
    .execute(pool)
    .await
    .unwrap();
}

async fn insert_metric_at(
    pool: &sqlx::PgPool,
    name: &str,
    service: Option<&str>,
    value: f64,
    secs_ago: f64,
) {
    sqlx::query(
        "INSERT INTO metrics (time, service, name, kind, value, unit)
         VALUES (now() - make_interval(secs => $1), $2, $3, 'gauge', $4, '1')",
    )
    .bind(secs_ago)
    .bind(service)
    .bind(name)
    .bind(value)
    .execute(pool)
    .await
    .unwrap();
}

async fn insert_rollup_at(pool: &sqlx::PgPool, name: &str, secs_ago: f64) {
    sqlx::query(
        "INSERT INTO metric_series_rollups
             (bucket, name, series_key, attrs, kind, count, sum, min, max, avg)
         VALUES (now() - make_interval(secs => $1), $2, md5($2), '{}', 'gauge', 1, 1, 1, 1, 1)",
    )
    .bind(secs_ago)
    .bind(name)
    .execute(pool)
    .await
    .unwrap();
}

async fn count(pool: &sqlx::PgPool, table: &str) -> i64 {
    sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT count(*) FROM {table}")))
        .fetch_one(pool)
        .await
        .unwrap()
}

/// Spawn a tiny HTTP server that records every JSON body it receives.
async fn spawn_webhook() -> (
    String,
    std::sync::Arc<tokio::sync::Mutex<Vec<serde_json::Value>>>,
) {
    use axum::{routing::post, Json, Router};
    let store = std::sync::Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let s2 = store.clone();
    let app = Router::new().route(
        "/hook",
        post(move |Json(v): Json<serde_json::Value>| {
            let s = s2.clone();
            async move {
                s.lock().await.push(v);
                "ok"
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}/hook"), store)
}

// --- Traces ----------------------------------------------------------------

#[tokio::test]
#[serial]
async fn trace_counts_spans_and_errors() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    // Two spans in one trace; the child errors (status_code = 2).
    insert_span_at(&pool, "checkout", "tr1", "s1", 5.0).await;
    sqlx::query(
        "INSERT INTO spans (trace_id, span_id, parent_span_id, service, name,
             start_time, end_time, duration_ms, status_code)
         VALUES ('tr1','s2','s1','checkout','child', now(), now(), 2.0, 2)",
    )
    .execute(&pool)
    .await
    .unwrap();

    let router = app(pool);
    let (status, traces) = get_json(&router, "/api/traces").await;
    assert_eq!(status, StatusCode::OK);
    let arr = traces.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["span_count"], 2);
    assert_eq!(arr[0]["error_count"], 1);
}

#[tokio::test]
#[serial]
async fn traces_service_filter_and_limit() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    insert_span_at(&pool, "alpha", "a", "a1", 1.0).await;
    insert_span_at(&pool, "beta", "b", "b1", 1.0).await;
    let router = app(pool);

    let (_, only_alpha) = get_json(&router, "/api/traces?service=alpha").await;
    assert_eq!(only_alpha.as_array().unwrap().len(), 1);
    assert_eq!(only_alpha[0]["service"], "alpha");

    let (_, capped) = get_json(&router, "/api/traces?limit=1").await;
    assert_eq!(capped.as_array().unwrap().len(), 1);
}

#[tokio::test]
#[serial]
async fn traces_filter_by_name_attr_errors_and_duration() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    // Three single-span traces with distinct name / attr / duration / status.
    sqlx::query(
        "INSERT INTO spans (trace_id, span_id, service, name, start_time, end_time, duration_ms, status_code, attributes) VALUES
            ('tf','s','api','GET /health',     now()-interval '60 s', now()-interval '60 s' + interval '10 ms',  10, 0, '{\"http.method\":\"GET\"}'),
            ('ts','s','api','POST /checkout',  now()-interval '60 s', now()-interval '60 s' + interval '500 ms', 500,0, '{\"http.method\":\"POST\"}'),
            ('te','s','worker','process job',  now()-interval '60 s', now()-interval '60 s' + interval '50 ms',  50, 2, '{\"http.method\":\"POST\"}')",
    )
    .execute(&pool)
    .await
    .unwrap();
    let router = app(pool);

    let ids = |v: &serde_json::Value| {
        v.as_array()
            .unwrap()
            .iter()
            .map(|t| t["trace_id"].as_str().unwrap().to_string())
            .collect::<Vec<_>>()
    };

    // Baseline: all three.
    let (_, all) = get_json(&router, "/api/traces").await;
    assert_eq!(all.as_array().unwrap().len(), 3);

    // Root-name substring.
    let (_, byname) = get_json(&router, "/api/traces?name=checkout").await;
    assert_eq!(ids(&byname), vec!["ts"]);

    // Errors only.
    let (_, errs) = get_json(&router, "/api/traces?errors_only=true").await;
    assert_eq!(ids(&errs), vec!["te"]);
    assert_eq!(errs[0]["error_count"], 1);

    // Min duration — only the 500ms trace clears 100ms.
    let (_, slow) = get_json(&router, "/api/traces?min_duration_ms=100").await;
    assert_eq!(ids(&slow), vec!["ts"]);

    // Attribute equality (value's '=' is %3D-encoded in the query string).
    let (_, get_only) = get_json(&router, "/api/traces?attr=http.method%3DGET").await;
    assert_eq!(ids(&get_only), vec!["tf"]);

    // Filters compose: POST traces at least 100ms → just the checkout one.
    let (_, post_slow) = get_json(
        &router,
        "/api/traces?attr=http.method%3DPOST&min_duration_ms=100",
    )
    .await;
    assert_eq!(ids(&post_slow), vec!["ts"]);
}

#[tokio::test]
#[serial]
async fn get_trace_returns_spans_in_order() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    insert_span_at(&pool, "svc", "trace-x", "later", 1.0).await;
    insert_span_at(&pool, "svc", "trace-x", "earlier", 10.0).await;
    let router = app(pool);

    let (status, spans) = get_json(&router, "/api/traces/trace-x").await;
    assert_eq!(status, StatusCode::OK);
    let arr = spans.as_array().unwrap();
    assert_eq!(arr.len(), 2);
    // Ordered by start_time ASC → the 10s-ago span comes first.
    assert_eq!(arr[0]["span_id"], "earlier");
    assert_eq!(arr[1]["span_id"], "later");
}

// --- Logs ------------------------------------------------------------------

#[tokio::test]
#[serial]
async fn logs_filter_by_service_and_trace() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    insert_log_at(&pool, "api", 1.0).await;
    insert_log_at(&pool, "worker", 1.0).await;
    sqlx::query("INSERT INTO logs (time, service, trace_id, body) VALUES (now(),'api','corr','y')")
        .execute(&pool)
        .await
        .unwrap();
    let router = app(pool);

    let (_, api_logs) = get_json(&router, "/api/logs?service=api").await;
    assert_eq!(api_logs.as_array().unwrap().len(), 2);

    let (_, corr) = get_json(&router, "/api/logs?trace_id=corr").await;
    assert_eq!(corr.as_array().unwrap().len(), 1);
    assert_eq!(corr[0]["trace_id"], "corr");
}

#[tokio::test]
#[serial]
async fn logs_filter_by_span() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    // Two spans in the same trace, each with one log; a third log with no span.
    sqlx::query(
        "INSERT INTO logs (time, service, trace_id, span_id, body) VALUES
            (now(),'api','corr','span-a','a'),
            (now(),'api','corr','span-b','b'),
            (now(),'api','corr',NULL,'c')",
    )
    .execute(&pool)
    .await
    .unwrap();
    let router = app(pool);

    // span_id narrows to that one span's logs.
    let (status, only_a) = get_json(&router, "/api/logs?trace_id=corr&span_id=span-a").await;
    assert_eq!(status, StatusCode::OK);
    let arr = only_a.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["body"], "a");
    assert_eq!(arr[0]["span_id"], "span-a");

    // Without span_id the whole trace's logs come back.
    let (_, all) = get_json(&router, "/api/logs?trace_id=corr").await;
    assert_eq!(all.as_array().unwrap().len(), 3);
}

// --- Metrics ---------------------------------------------------------------

#[tokio::test]
#[serial]
async fn metrics_summary_kinds_and_filter() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    // A sum and a histogram, plus a gauge on another service.
    sqlx::query(
        "INSERT INTO metrics (time, service, name, kind, value, unit) VALUES
            (now(),'api','reqs','sum',5,'1'),
            (now(),'api','reqs','sum',7,'1')",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO metrics (time, service, name, kind, value, count, unit)
         VALUES (now(),'api','latency','histogram',12.5,3,'ms')",
    )
    .execute(&pool)
    .await
    .unwrap();
    insert_metric_at(&pool, "cpu", Some("worker"), 0.5, 1.0).await;
    let router = app(pool);

    let (_, all) = get_json(&router, "/api/metrics").await;
    assert_eq!(all.as_array().unwrap().len(), 3); // reqs, latency, cpu

    let (_, api_only) = get_json(&router, "/api/metrics?service=api").await;
    let names: Vec<&str> = api_only
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"reqs"));
    assert!(names.contains(&"latency"));
    assert!(!names.contains(&"cpu"));
    let reqs = api_only
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["name"] == "reqs")
        .unwrap();
    assert_eq!(reqs["kind"], "sum");
    // Last value is the most recent sum point (both share now(), so either).
    let lv = reqs["last_value"].as_f64().unwrap();
    assert!(lv == 5.0 || lv == 7.0, "last_value was {lv}");
}

#[tokio::test]
#[serial]
async fn metrics_summary_sums_across_series() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    // One metric, two series distinguished by attributes (e.g. per-pod). The
    // summary reports how many distinct series the row folds together via
    // series_count, so the list can flag it and point at the chart's breakdown.
    sqlx::query(
        "INSERT INTO metrics (time, service, name, kind, value, unit, attributes) VALUES
            (now(),'api','mem','gauge',10,'By','{\"pod\":\"a\"}'),
            (now(),'api','mem','gauge',20,'By','{\"pod\":\"b\"}')",
    )
    .execute(&pool)
    .await
    .unwrap();
    let router = app(pool);

    let (_, all) = get_json(&router, "/api/metrics").await;
    let mem = all
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["name"] == "mem")
        .expect("mem row");
    // Latest single point (both share now(), so either of the two series).
    let lv = mem["last_value"].as_f64().unwrap();
    assert!(lv == 10.0 || lv == 20.0, "last_value was {lv}");
    assert_eq!(mem["series_count"].as_i64().unwrap(), 2);
}

#[tokio::test]
#[serial]
async fn metric_facet_splits_series_and_rates_a_counter() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    // A monotonic sum (counter) with two series across two buckets. Ingested via
    // the real path, so aggregate-on-insert builds the per-series rollups the
    // facet reads. It should return one series each and present per-second rates,
    // not raw cumulatives.
    let router = app(pool);
    for (pod, value, secs) in [
        ("a", 100.0, 900),
        ("a", 160.0, 600),
        ("b", 500.0, 900),
        ("b", 500.0, 600),
    ] {
        ingest(
            &router,
            one_number(
                "reqs.total",
                Some(true),
                "api",
                Some(pod),
                value,
                nanos_ago(secs),
            ),
        )
        .await;
    }

    let (status, f) = get_json(&router, "/api/metrics/facet?name=reqs.total&hours=1").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(f["rated"], true, "monotonic sum should be rated");
    assert_eq!(f["kind"], "sum");
    let series = f["series"].as_array().unwrap();
    assert_eq!(series.len(), 2, "one series per pod");
    // pod=a climbed 100->160 over ~600s => a positive rate somewhere; pod=b flat => 0.
    let max_rate = |s: &serde_json::Value| {
        s["points"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|p| p["v"].as_f64())
            .fold(0.0_f64, f64::max)
    };
    let a = series.iter().find(|s| s["attrs"]["pod"] == "a").unwrap();
    let b = series.iter().find(|s| s["attrs"]["pod"] == "b").unwrap();
    assert!(
        max_rate(a) > 0.0,
        "counter pod=a should have a positive rate"
    );
    assert_eq!(max_rate(b), 0.0, "flat counter pod=b rate is 0");
}

#[tokio::test]
#[serial]
async fn metric_histogram_interpolates_percentiles() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    // 100 observations all in the (10,20] bucket. Ingested via the real path so
    // the per-series histogram rollup the endpoint reads is built on insert.
    // Linear interpolation puts the median at the bucket midpoint (15) and p95
    // near the top (19.5).
    let router = app(pool);
    ingest(
        &router,
        one_histogram(
            "lat",
            "api",
            None,
            vec![10.0, 20.0, 30.0],
            vec![0, 100, 0, 0],
            100,
            1500.0,
            nanos_ago(600),
        ),
    )
    .await;

    let (status, h) = get_json(&router, "/api/metrics/histogram?name=lat&hours=1").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(h["bounds"], serde_json::json!([10.0, 20.0, 30.0]));
    let b = &h["buckets"].as_array().unwrap()[0];
    let p50 = b["p50"].as_f64().unwrap();
    let p95 = b["p95"].as_f64().unwrap();
    assert!((p50 - 15.0).abs() < 0.001, "p50 was {p50}");
    assert!((p95 - 19.5).abs() < 0.001, "p95 was {p95}");
    assert_eq!(b["counts"], serde_json::json!([0, 100, 0, 0]));
}

#[tokio::test]
#[serial]
async fn metric_facet_reads_rollups_after_raw_pruned() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    // Ingest a 30-min-old gauge (so the rollup row is written on insert), then
    // delete the raw point (simulating raw retention). The faceted view must
    // still find the series in the rollup.
    let router = app(pool.clone());
    ingest(
        &router,
        one_number("gx", None, "api", Some("a"), 42.0, nanos_ago(1800)),
    )
    .await;
    sqlx::query("DELETE FROM metrics WHERE name='gx'")
        .execute(&pool)
        .await
        .unwrap();

    let (status, f) = get_json(&router, "/api/metrics/facet?name=gx&hours=2").await;
    assert_eq!(status, StatusCode::OK);
    let series = f["series"].as_array().unwrap();
    assert_eq!(series.len(), 1, "series survives in the rollup");
    let v = series[0]["points"]
        .as_array()
        .unwrap()
        .iter()
        .find_map(|p| p["v"].as_f64())
        .unwrap();
    assert_eq!(v, 42.0);
}

#[tokio::test]
#[serial]
async fn metric_hist_facet_returns_per_series_percentiles() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    // 100 observations in (10,20] for one series, ingested (rolled up on insert)
    // then raw pruned.
    let router = app(pool.clone());
    ingest(
        &router,
        one_histogram(
            "hl",
            "api",
            Some("a"),
            vec![10.0, 20.0, 30.0],
            vec![0, 100, 0, 0],
            100,
            1500.0,
            nanos_ago(1800),
        ),
    )
    .await;
    sqlx::query("DELETE FROM metrics WHERE name='hl'")
        .execute(&pool)
        .await
        .unwrap();

    let (status, h) = get_json(&router, "/api/metrics/hist_facet?name=hl&hours=2").await;
    assert_eq!(status, StatusCode::OK);
    let series = h["series"].as_array().unwrap();
    assert_eq!(series.len(), 1);
    let pt = &series[0]["points"].as_array().unwrap()[0];
    let p95 = pt["p95"].as_f64().unwrap();
    assert!((p95 - 19.5).abs() < 0.001, "p95 was {p95}");
}

#[tokio::test]
#[serial]
async fn ingest_aggregates_into_rollup_on_insert() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let router = app(pool.clone());

    // Two gauge points of the same series in the same bucket. Aggregation happens
    // on ingest (no sweep), so the rollup row should already hold the merged
    // count/sum/avg and the facet should read it live.
    let now = now_nanos();
    assert_eq!(
        post_proto(
            &router,
            "/v1/metrics",
            gauge_request("g.live", 10.0, now).encode_to_vec()
        )
        .await,
        StatusCode::OK
    );
    assert_eq!(
        post_proto(
            &router,
            "/v1/metrics",
            gauge_request("g.live", 20.0, now).encode_to_vec()
        )
        .await,
        StatusCode::OK
    );

    let (count, sum, avg): (i64, f64, f64) =
        sqlx::query_as("SELECT count, sum, avg FROM metric_series_rollups WHERE name = 'g.live'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(count, 2, "two points merged into one rollup row");
    assert_eq!(sum, 30.0);
    assert_eq!(avg, 15.0, "avg derived from merged sum/count");

    // No rollup_once call — the facet reads the live rollup straight away.
    let (status, f) = get_json(&router, "/api/metrics/facet?name=g.live&hours=1").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(f["series"].as_array().unwrap().len(), 1);
}

#[tokio::test]
#[serial]
async fn batched_numbers_aggregate_within_one_request() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let router = app(pool.clone());
    let now = now_nanos();

    // A single request with three gauge points: pod=a twice (same series + bucket,
    // so the batch must pre-aggregate them) and pod=b once (a separate series).
    assert_eq!(
        post_proto(
            &router,
            "/v1/metrics",
            gauge_points_request("gb", &[("a", 10.0), ("a", 20.0), ("b", 5.0)], now)
                .encode_to_vec()
        )
        .await,
        StatusCode::OK
    );

    // Every raw point is still kept.
    let raw: i64 = sqlx::query_scalar("SELECT count(*) FROM metrics WHERE name='gb'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(raw, 3);

    // One rollup row per series; pod=a's two points merged inside the batch.
    let rows: i64 =
        sqlx::query_scalar("SELECT count(*) FROM metric_series_rollups WHERE name='gb'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(rows, 2, "one rollup row per series");
    let (count, sum, avg): (i64, f64, f64) = sqlx::query_as(
        "SELECT count, sum, avg FROM metric_series_rollups WHERE name='gb' AND attrs->>'pod'='a'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((count, sum, avg), (2, 30.0, 15.0), "pod=a merged 10+20");
}

#[tokio::test]
#[serial]
async fn batched_histograms_sum_within_one_request() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let router = app(pool.clone());
    let now = now_nanos();

    // Two histogram points of one series in a single request: their bucket_counts
    // must be summed element-wise (array_sum) and their observation counts added.
    let req = ExportMetricsServiceRequest {
        resource_metrics: vec![ResourceMetrics {
            resource: Some(Resource {
                attributes: vec![kv("service.name", "api")],
                ..Default::default()
            }),
            scope_metrics: vec![ScopeMetrics {
                metrics: vec![Metric {
                    name: "hb".to_string(),
                    unit: "ms".to_string(),
                    data: Some(metric::Data::Histogram(Histogram {
                        aggregation_temporality: 0,
                        data_points: vec![
                            HistogramDataPoint {
                                time_unix_nano: now,
                                count: 3,
                                sum: Some(30.0),
                                explicit_bounds: vec![10.0, 20.0],
                                bucket_counts: vec![1, 2, 0],
                                ..Default::default()
                            },
                            HistogramDataPoint {
                                time_unix_nano: now,
                                count: 3,
                                sum: Some(30.0),
                                explicit_bounds: vec![10.0, 20.0],
                                bucket_counts: vec![0, 1, 2],
                                ..Default::default()
                            },
                        ],
                    })),
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        }],
    };
    assert_eq!(
        post_proto(&router, "/v1/metrics", req.encode_to_vec()).await,
        StatusCode::OK
    );

    let counts: Vec<i64> =
        sqlx::query_scalar("SELECT bucket_counts FROM metric_series_rollups WHERE name='hb'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(counts, vec![1, 3, 2], "[1,2,0] + [0,1,2] element-wise");
    let count: i64 = sqlx::query_scalar("SELECT count FROM metric_series_rollups WHERE name='hb'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 6, "3 + 3 observations");
}

// --- Rollups ---------------------------------------------------------------

#[tokio::test]
#[serial]
async fn series_collapses_per_bucket_without_double_count() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    // Two points in two different buckets (30 min ago and now), both folded into
    // the per-series rollup on ingest. The collapsed series must return exactly
    // one value per bucket — the rollup branch and the recent-raw stitch must not
    // double-count the same bucket.
    let router = app(pool);
    ingest(
        &router,
        one_number("lat", None, "api", None, 10.0, nanos_ago(1800)),
    )
    .await;
    ingest(
        &router,
        one_number("lat", None, "api", None, 20.0, nanos_ago(0)),
    )
    .await;

    let (status, series) = get_json(&router, "/api/metrics/series?name=lat&hours=2").await;
    assert_eq!(status, StatusCode::OK);
    let arr = series.as_array().unwrap();
    assert_eq!(arr.len(), 2, "one value per bucket, series = {arr:?}");
    assert_eq!(arr[0]["v"], 10.0);
    assert_eq!(arr[1]["v"], 20.0);
}

// --- Retention -------------------------------------------------------------

#[tokio::test]
#[serial]
async fn retention_prunes_raw_metrics_before_rollups() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let day = 86_400.0;
    let hour = 3_600.0;
    // spans/logs: old gone, recent kept (window = 7d).
    insert_span_at(&pool, "svc", "old", "o", 10.0 * day).await;
    insert_span_at(&pool, "svc", "new", "n", 1.0 * day).await;
    insert_log_at(&pool, "svc", 10.0 * day).await;
    insert_log_at(&pool, "svc", 1.0 * day).await;
    // raw metrics: window = 6h, so the 10-hour-old point goes, the 1-hour stays.
    insert_metric_at(&pool, "m", None, 1.0, 10.0 * hour).await;
    insert_metric_at(&pool, "m", None, 1.0, 1.0 * hour).await;
    // per-series rollups: window = 7d, so the 10-day rollup goes, the 3-day stays.
    insert_rollup_at(&pool, "m", 10.0 * day).await;
    insert_rollup_at(&pool, "m", 3.0 * day).await;

    let deleted = watcher_server::retention::prune_once(
        &pool,
        7,
        6,
        watcher_server::retention::Windows::default(),
    )
    .await
    .unwrap();
    assert!(deleted >= 4);

    assert_eq!(count(&pool, "spans").await, 1);
    assert_eq!(count(&pool, "logs").await, 1);
    assert_eq!(count(&pool, "metrics").await, 1);
    assert_eq!(count(&pool, "metric_series_rollups").await, 1);
}

/// Per-table windows (JEF-434): spans get a short window, logs a long one, and
/// metric rollups fall back to the global default (no override at all) — each
/// table must prune to its own cutoff, not the global one.
#[tokio::test]
#[serial]
async fn retention_prunes_each_table_to_its_own_window() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let day = 86_400.0;
    // spans: window = 1d override, so the 2-day-old row goes, the 12h row stays.
    insert_span_at(&pool, "svc", "old", "o", 2.0 * day).await;
    insert_span_at(&pool, "svc", "new", "n", 0.5 * day).await;
    // logs: window = 30d override, so both the 2-day and 20-day rows survive a
    // sweep that would prune them under the 1d/7d windows above.
    insert_log_at(&pool, "svc", 2.0 * day).await;
    insert_log_at(&pool, "svc", 20.0 * day).await;
    // rollups: no override, falls back to the global default (7d) — the 10-day
    // row goes, the 3-day row stays.
    insert_rollup_at(&pool, "m", 10.0 * day).await;
    insert_rollup_at(&pool, "m", 3.0 * day).await;

    let windows = watcher_server::retention::Windows {
        spans_days: Some(1),
        logs_days: Some(30),
        metrics_days: None,
    };
    watcher_server::retention::prune_once(&pool, 7, 6, windows)
        .await
        .unwrap();

    assert_eq!(
        count(&pool, "spans").await,
        1,
        "1d window: only the recent span survives"
    );
    assert_eq!(
        count(&pool, "logs").await,
        2,
        "30d window: both logs survive"
    );
    assert_eq!(
        count(&pool, "metric_series_rollups").await,
        1,
        "falls back to the 7d global default"
    );
}

/// A backlog larger than one batch must fully drain. `batch = 1` forces the loop
/// to iterate per row, so this fails if the prune stops after a single statement.
#[tokio::test]
#[serial]
async fn retention_raw_metrics_drains_in_batches() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let hour = 3_600.0;
    // Three points older than the 6h window, one inside it.
    for _ in 0..3 {
        insert_metric_at(&pool, "m", None, 1.0, 10.0 * hour).await;
    }
    insert_metric_at(&pool, "m", None, 1.0, 1.0 * hour).await;

    let pruned = watcher_server::retention::prune_raw_metrics(&pool, 6, 1)
        .await
        .unwrap();
    assert_eq!(pruned, 3);
    assert_eq!(count(&pool, "metrics").await, 1);
}

// --- Alerts ----------------------------------------------------------------

/// Apply declared rules through the real reconcile path. Rules are declarative
/// now (no create API), so the whole declared set goes in one call — reconcile
/// upserts these and prunes anything else.
async fn apply_rules(pool: &sqlx::PgPool, rules: &[serde_json::Value]) {
    let cfgs: Vec<alerts::RuleConfig> = rules
        .iter()
        .map(|v| serde_json::from_value(v.clone()).expect("rule config"))
        .collect();
    alerts::reconcile(pool, &cfgs).await.expect("reconcile");
}

#[tokio::test]
#[serial]
async fn alert_fires_then_resolves() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let router = app(pool.clone());
    apply_rules(
        &pool,
        &[serde_json::json!({"name":"hot","metric":"t","comparator":"gt","threshold":50,"window_secs":3600})],
    )
    .await;

    insert_metric_at(&pool, "t", None, 90.0, 1.0).await;
    alerts::evaluate_once(&pool, None).await.unwrap();
    let (_, rules) = get_json(&router, "/api/alerts").await;
    assert_eq!(rules[0]["firing"], true);

    // Recover below threshold; a fresh window of low values resolves it.
    sqlx::query("DELETE FROM metrics WHERE name = 't'")
        .execute(&pool)
        .await
        .unwrap();
    insert_metric_at(&pool, "t", None, 5.0, 1.0).await;
    alerts::evaluate_once(&pool, None).await.unwrap();
    let (_, rules) = get_json(&router, "/api/alerts").await;
    assert_eq!(rules[0]["firing"], false);

    let (_, events) = get_json(&router, "/api/alerts/events").await;
    assert_eq!(events.as_array().unwrap().len(), 1); // one fire/resolve cycle
    assert!(!events[0]["resolved_at"].is_null());
}

#[tokio::test]
#[serial]
async fn alert_lt_and_max_agg() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let router = app(pool.clone());

    apply_rules(
        &pool,
        &[
            // lt rule fires when the value drops below the floor.
            serde_json::json!({"name":"cold","metric":"temp","comparator":"lt","threshold":0,"window_secs":3600}),
            // max-agg rule: avg would be 55 (< 80) but max is 100 (> 80) → fires.
            serde_json::json!({"name":"spike","metric":"q","comparator":"gt","threshold":80,"agg":"max","window_secs":3600}),
        ],
    )
    .await;

    insert_metric_at(&pool, "temp", None, -5.0, 1.0).await;
    insert_metric_at(&pool, "q", None, 10.0, 1.0).await;
    insert_metric_at(&pool, "q", None, 100.0, 2.0).await;
    alerts::evaluate_once(&pool, None).await.unwrap();

    let (_, rules) = get_json(&router, "/api/alerts").await;
    for r in rules.as_array().unwrap() {
        assert_eq!(r["firing"], true, "rule {} should fire", r["name"]);
    }
}

#[tokio::test]
#[serial]
async fn alert_disabled_rule_not_evaluated() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    sqlx::query(
        "INSERT INTO alert_rules (name, metric, comparator, threshold, agg, window_secs, enabled)
         VALUES ('off','t','gt',1,'avg',3600,false)",
    )
    .execute(&pool)
    .await
    .unwrap();
    insert_metric_at(&pool, "t", None, 1000.0, 1.0).await;
    alerts::evaluate_once(&pool, None).await.unwrap();
    assert_eq!(count(&pool, "alert_events").await, 0);
}

#[tokio::test]
#[serial]
async fn alert_does_not_fire_twice() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    apply_rules(
        &pool,
        &[serde_json::json!({"name":"x","metric":"t","comparator":"gt","threshold":1,"window_secs":3600})],
    )
    .await;
    insert_metric_at(&pool, "t", None, 9.0, 1.0).await;
    alerts::evaluate_once(&pool, None).await.unwrap();
    alerts::evaluate_once(&pool, None).await.unwrap(); // still breaching
    assert_eq!(count(&pool, "alert_events").await, 1); // one open event only
}

/// Age the (single) open event for a rule backwards so its dwell window has
/// elapsed without waiting real time — the evaluator keys maturity off fired_at.
async fn age_open_event(pool: &sqlx::PgPool, secs: f64) {
    sqlx::query(
        "UPDATE alert_events SET fired_at = now() - make_interval(secs => $1)
         WHERE resolved_at IS NULL",
    )
    .bind(secs)
    .execute(pool)
    .await
    .unwrap();
}

#[tokio::test]
#[serial]
async fn alert_for_secs_requires_sustained_breach() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let router = app(pool.clone());
    apply_rules(
        &pool,
        &[
            serde_json::json!({"name":"dwell","metric":"t","comparator":"gt",
                             "threshold":50,"window_secs":3600,"for_secs":300}),
        ],
    )
    .await;

    // First breach: a pending event opens but the rule does NOT fire yet.
    insert_metric_at(&pool, "t", None, 90.0, 1.0).await;
    alerts::evaluate_once(&pool, None).await.unwrap();
    let (_, rules) = get_json(&router, "/api/alerts").await;
    assert_eq!(rules[0]["firing"], false, "pending breach must not fire");
    assert_eq!(count(&pool, "alert_events").await, 1); // pending row exists
    let (_, events) = get_json(&router, "/api/alerts/events").await;
    assert!(
        events.as_array().unwrap().is_empty(),
        "a pending event is not a transition"
    );

    // Still breaching a second tick before the window elapses: still pending.
    alerts::evaluate_once(&pool, None).await.unwrap();
    let (_, rules) = get_json(&router, "/api/alerts").await;
    assert_eq!(rules[0]["firing"], false);

    // The breach has now held past for_secs → it activates and fires.
    age_open_event(&pool, 301.0).await;
    alerts::evaluate_once(&pool, None).await.unwrap();
    let (_, rules) = get_json(&router, "/api/alerts").await;
    assert_eq!(rules[0]["firing"], true, "matured breach must fire");
    assert_eq!(count(&pool, "alert_events").await, 1); // reused the same event
    let (_, events) = get_json(&router, "/api/alerts/events").await;
    assert_eq!(events.as_array().unwrap().len(), 1); // now a real transition

    // Resolve semantics are unchanged: recovery closes the firing event.
    sqlx::query("DELETE FROM metrics WHERE name = 't'")
        .execute(&pool)
        .await
        .unwrap();
    insert_metric_at(&pool, "t", None, 5.0, 1.0).await;
    alerts::evaluate_once(&pool, None).await.unwrap();
    let (_, rules) = get_json(&router, "/api/alerts").await;
    assert_eq!(rules[0]["firing"], false);
    let (_, events) = get_json(&router, "/api/alerts/events").await;
    assert!(!events[0]["resolved_at"].is_null());
}

#[tokio::test]
#[serial]
async fn alert_for_secs_flap_never_fires() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let router = app(pool.clone());
    apply_rules(
        &pool,
        &[
            serde_json::json!({"name":"flap","metric":"t","comparator":"gt",
                             "threshold":50,"window_secs":3600,"for_secs":300}),
        ],
    )
    .await;

    // Breach opens a pending event (no fire).
    insert_metric_at(&pool, "t", None, 90.0, 1.0).await;
    alerts::evaluate_once(&pool, None).await.unwrap();
    assert_eq!(count(&pool, "alert_events").await, 1);
    let (_, rules) = get_json(&router, "/api/alerts").await;
    assert_eq!(rules[0]["firing"], false);

    // The breach clears well before the 5-min window: the pending event is dropped
    // silently — no firing, no resolved transition, nothing paged.
    sqlx::query("DELETE FROM metrics WHERE name = 't'")
        .execute(&pool)
        .await
        .unwrap();
    insert_metric_at(&pool, "t", None, 5.0, 1.0).await;
    alerts::evaluate_once(&pool, None).await.unwrap();
    assert_eq!(
        count(&pool, "alert_events").await,
        0,
        "pending event dropped"
    );
    let (_, rules) = get_json(&router, "/api/alerts").await;
    assert_eq!(rules[0]["firing"], false);
    let (_, events) = get_json(&router, "/api/alerts/events").await;
    assert!(events.as_array().unwrap().is_empty());

    // A fresh breach after the flap starts a brand-new dwell (and still won't fire
    // until it too holds long enough) — proving the flap left no residual state.
    sqlx::query("DELETE FROM metrics WHERE name = 't'")
        .execute(&pool)
        .await
        .unwrap();
    insert_metric_at(&pool, "t", None, 95.0, 1.0).await;
    alerts::evaluate_once(&pool, None).await.unwrap();
    assert_eq!(count(&pool, "alert_events").await, 1);
    let (_, rules) = get_json(&router, "/api/alerts").await;
    assert_eq!(rules[0]["firing"], false);
    age_open_event(&pool, 400.0).await;
    alerts::evaluate_once(&pool, None).await.unwrap();
    let (_, rules) = get_json(&router, "/api/alerts").await;
    assert_eq!(rules[0]["firing"], true);
}

#[tokio::test]
#[serial]
async fn alert_webhook_delivers_payload() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let (url, store) = spawn_webhook().await;
    sqlx::query(
        "INSERT INTO alert_rules (name, metric, comparator, threshold, agg, window_secs)
         VALUES ('wh','t','gt',1,'avg',3600)",
    )
    .execute(&pool)
    .await
    .unwrap();
    insert_metric_at(&pool, "t", None, 7.0, 1.0).await;

    alerts::evaluate_once(&pool, Some(&url)).await.unwrap();

    let received = store.lock().await;
    assert_eq!(received.len(), 1);
    assert_eq!(received[0]["state"], "firing");
    assert_eq!(received[0]["rule"], "wh");
    assert_eq!(received[0]["value"], 7.0);
}

#[tokio::test]
#[serial]
async fn alert_reconcile_rejects_invalid_rules() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };

    // A bad agg, a bad comparator, and an empty name each abort the whole apply
    // — and because validation runs before any write, a rejected batch leaves the
    // table untouched (no partial state).
    let bad_agg = serde_json::from_value::<alerts::RuleConfig>(
        serde_json::json!({"name":"x","metric":"t","comparator":"gt","threshold":1,"agg":"median"}),
    )
    .unwrap();
    assert!(alerts::reconcile(&pool, std::slice::from_ref(&bad_agg))
        .await
        .is_err());

    let empty_name = serde_json::from_value::<alerts::RuleConfig>(
        serde_json::json!({"name":"","metric":"t","comparator":"gt","threshold":1}),
    )
    .unwrap();
    assert!(alerts::reconcile(&pool, std::slice::from_ref(&empty_name))
        .await
        .is_err());

    assert_eq!(count(&pool, "alert_rules").await, 0);
}

#[tokio::test]
#[serial]
async fn alert_reconcile_upserts_in_place() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let router = app(pool.clone());

    // Declare a rule and fire it, opening an event.
    apply_rules(
        &pool,
        &[serde_json::json!({"name":"hot","metric":"t","comparator":"gt","threshold":50,"window_secs":3600})],
    )
    .await;
    insert_metric_at(&pool, "t", None, 90.0, 1.0).await;
    alerts::evaluate_once(&pool, None).await.unwrap();
    let (_, rules) = get_json(&router, "/api/alerts").await;
    let id_before = rules[0]["id"].as_i64().unwrap();
    assert_eq!(count(&pool, "alert_events").await, 1);

    // Re-declare the same rule with a new threshold: it's upserted by name, so
    // the id (and its open event) survive rather than being dropped + recreated.
    apply_rules(
        &pool,
        &[serde_json::json!({"name":"hot","metric":"t","comparator":"gt","threshold":75,"window_secs":3600})],
    )
    .await;
    let (_, rules) = get_json(&router, "/api/alerts").await;
    assert_eq!(rules.as_array().unwrap().len(), 1);
    assert_eq!(rules[0]["id"].as_i64().unwrap(), id_before);
    assert_eq!(rules[0]["threshold"], 75.0);
    assert_eq!(count(&pool, "alert_events").await, 1); // event preserved
}

/// Insert one metric point with a JSONB attribute set (and optional counter
/// monotonicity), so alert-rule match/exclude/rate paths can be exercised directly.
async fn insert_metric_attr(
    pool: &sqlx::PgPool,
    name: &str,
    attrs: serde_json::Value,
    kind: &str,
    is_monotonic: Option<bool>,
    value: f64,
    secs_ago: f64,
) {
    sqlx::query(
        "INSERT INTO metrics (time, service, name, kind, value, unit, attributes, is_monotonic)
         VALUES (now() - make_interval(secs => $1), 'api', $2, $3, $4, '1', $5, $6)",
    )
    .bind(secs_ago)
    .bind(name)
    .bind(kind)
    .bind(value)
    .bind(attrs)
    .bind(is_monotonic)
    .execute(pool)
    .await
    .unwrap();
}

/// Look up a rule's current firing state from the read API by name.
fn firing(rules: &serde_json::Value, name: &str) -> bool {
    rules
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["name"] == name)
        .unwrap_or_else(|| panic!("rule {name} not found"))["firing"]
        .as_bool()
        .unwrap()
}

#[tokio::test]
#[serial]
async fn alert_match_scopes_to_matching_series() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let router = app(pool.clone());

    // Two series of the same metric: only the primary is quiet, the replica is hot.
    insert_metric_attr(
        &pool,
        "cpu",
        json!({"role":"primary"}),
        "gauge",
        None,
        10.0,
        5.0,
    )
    .await;
    insert_metric_attr(
        &pool,
        "cpu",
        json!({"role":"replica"}),
        "gauge",
        None,
        90.0,
        5.0,
    )
    .await;

    apply_rules(
        &pool,
        &[
            // Scoped to the primary series: max is 10 (< 80) → does NOT fire.
            json!({"name":"scoped","metric":"cpu","comparator":"gt","threshold":80,
                   "agg":"max","window_secs":3600,"match":{"role":"primary"}}),
            // Unscoped control: max over both series is 90 (> 80) → fires.
            json!({"name":"broad","metric":"cpu","comparator":"gt","threshold":80,
                   "agg":"max","window_secs":3600}),
        ],
    )
    .await;
    alerts::evaluate_once(&pool, None).await.unwrap();

    let (_, rules) = get_json(&router, "/api/alerts").await;
    assert!(
        !firing(&rules, "scoped"),
        "match must narrow to the primary series"
    );
    assert!(
        firing(&rules, "broad"),
        "unscoped rule still fires on the replica"
    );
}

#[tokio::test]
#[serial]
async fn alert_exclude_suppresses_series() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let router = app(pool.clone());

    // A ready Deployment pod and an unready Job pod. "container not ready" is lt 1.
    insert_metric_attr(
        &pool,
        "ready",
        json!({"owner":"Deployment"}),
        "gauge",
        None,
        1.0,
        5.0,
    )
    .await;
    insert_metric_attr(
        &pool,
        "ready",
        json!({"owner":"Job"}),
        "gauge",
        None,
        0.0,
        5.0,
    )
    .await;

    apply_rules(
        &pool,
        &[
            // Excluding Job leaves only the Deployment(1) → min 1, not < 1 → no fire.
            json!({"name":"exset","metric":"ready","comparator":"lt","threshold":1,
                   "agg":"min","window_secs":3600,"exclude":{"owner":"Job"}}),
            // Without the exclude, the Job(0) drags min to 0 → false page fires.
            json!({"name":"noexc","metric":"ready","comparator":"lt","threshold":1,
                   "agg":"min","window_secs":3600}),
        ],
    )
    .await;
    alerts::evaluate_once(&pool, None).await.unwrap();

    let (_, rules) = get_json(&router, "/api/alerts").await;
    assert!(
        !firing(&rules, "exset"),
        "exclude must suppress the Job series"
    );
    assert!(
        firing(&rules, "noexc"),
        "without exclude the Job series still fires"
    );
}

#[tokio::test]
#[serial]
async fn alert_rate_fires_on_per_second_rate() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let router = app(pool.clone());

    // A monotonic counter climbing 300 over 10s on one series → 30/s.
    let attrs = json!({"pod":"a"});
    insert_metric_attr(&pool, "reqs", attrs.clone(), "sum", Some(true), 100.0, 20.0).await;
    insert_metric_attr(&pool, "reqs", attrs.clone(), "sum", Some(true), 400.0, 10.0).await;

    apply_rules(
        &pool,
        &[
            // rate auto-on (monotonic sum): per-second rate is 30 (> 20) → fires.
            json!({"name":"rated","metric":"reqs","comparator":"gt","threshold":20,
                   "agg":"max","window_secs":3600}),
            // Explicit rate:false reads the raw cumulative level: max 400 (> 20).
            json!({"name":"raw","metric":"reqs","comparator":"gt","threshold":20,
                   "agg":"max","window_secs":3600,"rate":false}),
        ],
    )
    .await;
    alerts::evaluate_once(&pool, None).await.unwrap();

    let (_, rules) = get_json(&router, "/api/alerts").await;
    assert!(
        firing(&rules, "rated"),
        "counter rate 30/s fires the gt-20 rule"
    );
    assert!(firing(&rules, "raw"), "raw-level control also fires");

    // The stored event values distinguish the rate from the raw cumulative level.
    let (_, events) = get_json(&router, "/api/alerts/events").await;
    let val = |name: &str| -> f64 {
        events
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["rule_name"] == name)
            .unwrap()["value"]
            .as_f64()
            .unwrap()
    };
    // The differencing divides by the actual elapsed seconds between inserts (a few
    // ms over the nominal 10s), so the rate lands just under 30 — a wide band keeps
    // this asserting "rate, not raw level" without being timing-flaky.
    assert!(
        (val("rated") - 30.0).abs() < 1.0,
        "rate ≈ 30/s, got {}",
        val("rated")
    );
    assert!(
        (val("raw") - 400.0).abs() < 1e-6,
        "raw level 400, got {}",
        val("raw")
    );
}

#[tokio::test]
#[serial]
async fn alert_rate_reset_yields_zero_not_spike() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let router = app(pool.clone());

    // A counter that resets (process restart): level drops from 1_000_000 to 10.
    // The only interval is the reset, so the reset-safe rate is 0 — never a spike.
    let attrs = json!({"pod":"x"});
    insert_metric_attr(
        &pool,
        "creset",
        attrs.clone(),
        "sum",
        Some(true),
        1_000_000.0,
        20.0,
    )
    .await;
    insert_metric_attr(
        &pool,
        "creset",
        attrs.clone(),
        "sum",
        Some(true),
        10.0,
        10.0,
    )
    .await;

    apply_rules(
        &pool,
        &[
            // gt -1 with max agg: fires iff the rate is a real number ≥ 0. A correct
            // reset guard yields exactly 0 (fires, value 0); a naive (cur-prev)/dt
            // would be a large negative and NOT fire → this asserts the guard.
            json!({"name":"reset","metric":"creset","comparator":"gt","threshold":-1,
                   "agg":"max","window_secs":3600,"rate":true}),
        ],
    )
    .await;
    alerts::evaluate_once(&pool, None).await.unwrap();

    let (_, rules) = get_json(&router, "/api/alerts").await;
    assert!(
        firing(&rules, "reset"),
        "reset interval yields 0 (≥ -1), so the rule fires"
    );

    let (_, events) = get_json(&router, "/api/alerts/events").await;
    let value = events[0]["value"].as_f64().unwrap();
    assert_eq!(
        value, 0.0,
        "a reset must produce a 0 rate, not a spike (got {value})"
    );
}

#[tokio::test]
#[serial]
async fn ingest_gzipped_metric_keeps_resource_dimensions() {
    use std::io::Write;
    let Some(pool) = pool_or_skip().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let router = app(pool.clone());

    let req = ExportMetricsServiceRequest {
        resource_metrics: vec![ResourceMetrics {
            resource: Some(Resource {
                attributes: vec![
                    kv("service.name", "kubelet"),
                    kv("k8s.pod.name", "watcher-server-abc"),
                ],
                ..Default::default()
            }),
            scope_metrics: vec![ScopeMetrics {
                metrics: vec![Metric {
                    name: "container.memory.rss".to_string(),
                    data: Some(metric::Data::Gauge(Gauge {
                        data_points: vec![NumberDataPoint {
                            time_unix_nano: 1_000_000_000,
                            value: Some(number_data_point::Value::AsInt(1024)),
                            ..Default::default()
                        }],
                    })),
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        }],
    };

    // gzip-compress the OTLP body, as the OTel Collector / Traefik / SDKs do.
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    enc.write_all(&req.encode_to_vec()).unwrap();
    let gz = enc.finish().unwrap();

    let resp = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/metrics")
                .header("content-type", "application/x-protobuf")
                .header("content-encoding", "gzip")
                .body(Body::from(gz))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "gzipped OTLP should ingest");

    // The resource dimension must survive into the stored attributes.
    let pod: Option<String> = sqlx::query_scalar(
        "SELECT attributes->>'k8s.pod.name' FROM metrics WHERE name = 'container.memory.rss' LIMIT 1",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(pod.as_deref(), Some("watcher-server-abc"));
}

#[tokio::test]
#[serial]
async fn gzip_decompression_bomb_is_rejected() {
    use std::io::Write;
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let router = app(pool);

    // 80 MiB of zeros gzips to a tiny body (well under axum's 2 MB body limit) but
    // decompresses past the 64 MiB cap — payload() must reject it rather than
    // allocating it all. Without the cap this would balloon memory (a zip bomb).
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    enc.write_all(&vec![0u8; 80 * 1024 * 1024]).unwrap();
    let gz = enc.finish().unwrap();
    assert!(
        gz.len() < 2 * 1024 * 1024,
        "compressed bomb must clear the body limit to reach payload()"
    );

    let resp = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/metrics")
                .header("content-type", "application/x-protobuf")
                .header("content-encoding", "gzip")
                .body(Body::from(gz))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "an over-cap decompressed body must be rejected"
    );
}

#[tokio::test]
#[serial]
async fn ingest_log_keeps_resource_dimensions() {
    let Some(pool) = pool_or_skip().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let router = app(pool.clone());

    let req = ExportLogsServiceRequest {
        resource_logs: vec![ResourceLogs {
            resource: Some(Resource {
                attributes: vec![kv("service.name", "api"), kv("k8s.pod.name", "api-7d")],
                ..Default::default()
            }),
            scope_logs: vec![ScopeLogs {
                log_records: vec![LogRecord {
                    time_unix_nano: 1_000_000_000,
                    body: Some(AnyValue {
                        value: Some(any_value::Value::StringValue("hi".to_string())),
                    }),
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        }],
    };

    let resp = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/logs")
                .header("content-type", "application/x-protobuf")
                .body(Body::from(req.encode_to_vec()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let pod: Option<String> =
        sqlx::query_scalar("SELECT attributes->>'k8s.pod.name' FROM logs LIMIT 1")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(pod.as_deref(), Some("api-7d"));
}

#[tokio::test]
#[serial]
async fn time_window_filters_traces_and_logs() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let day = 86_400.0;
    insert_span_at(&pool, "svc", "recent", "r1", 30.0).await; // 30s ago
    insert_span_at(&pool, "svc", "old", "o1", 3.0 * day).await; // 3 days ago
    insert_log_at(&pool, "svc", 30.0).await;
    insert_log_at(&pool, "svc", 3.0 * day).await;
    let router = app(pool);

    // `from` an hour ago → only the recent rows. RFC3339 with Z (no '+').
    let from = (chrono::Utc::now() - chrono::Duration::hours(1))
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);

    let (status, traces) = get_json(&router, &format!("/api/traces?from={from}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(traces.as_array().unwrap().len(), 1, "only the recent trace");
    assert_eq!(traces[0]["trace_id"], "recent");

    let (_, logs) = get_json(&router, &format!("/api/logs?from={from}")).await;
    assert_eq!(logs.as_array().unwrap().len(), 1, "only the recent log");

    // No window → defaults to the recent (24h) window, so the 3-day-old trace is
    // excluded and only the recent one comes back.
    let (_, all) = get_json(&router, "/api/traces").await;
    assert_eq!(all.as_array().unwrap().len(), 1);
    assert_eq!(all[0]["trace_id"], "recent");
}

#[tokio::test]
#[serial]
async fn service_red_aggregates() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    // Four "api" spans: durations 10/20/30/40 ms, the last one an error.
    let rows = [(10.0, None), (20.0, None), (30.0, None), (40.0, Some(2))];
    for (i, (dur, code)) in rows.iter().enumerate() {
        sqlx::query(
            "INSERT INTO spans (trace_id, span_id, service, name, start_time, end_time, duration_ms, status_code)
             VALUES ($1,$2,'api','op', now(), now(), $3, $4)",
        )
        .bind(format!("t{i}"))
        .bind(format!("s{i}"))
        .bind(dur)
        .bind(*code as Option<i32>)
        .execute(&pool)
        .await
        .unwrap();
    }
    let router = app(pool);
    let (status, svcs) = get_json(&router, "/api/services").await;
    assert_eq!(status, StatusCode::OK);
    let arr = svcs.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    let s = &arr[0];
    assert_eq!(s["service"], "api");
    assert_eq!(s["spans"], 4);
    assert_eq!(s["errors"], 1);
    assert!((s["error_rate"].as_f64().unwrap() - 0.25).abs() < 1e-9);
    // percentile_cont(0.5) of [10,20,30,40] = 25 (interpolated).
    assert!((s["p50_ms"].as_f64().unwrap() - 25.0).abs() < 1e-6);
}

// JEF-532: an explicit `from` far beyond the max-lookback ceiling must not
// defeat it — the effective floor is clamped, not honored verbatim, so a
// full scan of the retention-deep `spans` table can't be forced.
#[tokio::test]
#[serial]
async fn traces_from_beyond_max_lookback_is_clamped() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let day = 86_400.0;
    // Lower the ceiling to 2 days so the test doesn't need to insert 7 days of
    // data; #[serial] keeps this process-global env change from racing other
    // tests.
    std::env::set_var("WATCHER_MAX_QUERY_HOURS", "48");
    insert_span_at(&pool, "svc", "recent", "r1", 3600.0).await; // 1h ago
    insert_span_at(&pool, "svc", "old", "o1", 10.0 * day).await; // 10 days ago
    let router = app(pool);

    // Ask for a window starting 30 days ago — far past the 48h ceiling.
    let from = (chrono::Utc::now() - chrono::Duration::days(30))
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let (status, traces) = get_json(&router, &format!("/api/traces?from={from}")).await;
    std::env::remove_var("WATCHER_MAX_QUERY_HOURS");

    assert_eq!(status, StatusCode::OK);
    let ids: Vec<_> = traces
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["trace_id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        ids,
        vec!["recent"],
        "the 10-day-old trace is outside the max-lookback ceiling and must stay \
         excluded even though `from` asked for 30 days back"
    );
}

// Same clamp, for /api/services' independent COALESCE(from, ...) guard.
#[tokio::test]
#[serial]
async fn services_from_beyond_max_lookback_is_clamped() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let day = 86_400.0;
    std::env::set_var("WATCHER_MAX_QUERY_HOURS", "48");
    insert_span_at(&pool, "svc", "recent", "r1", 3600.0).await; // 1h ago
    insert_span_at(&pool, "svc", "old", "o1", 10.0 * day).await; // 10 days ago
    let router = app(pool);

    let from = (chrono::Utc::now() - chrono::Duration::days(30))
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let (status, svcs) = get_json(&router, &format!("/api/services?from={from}")).await;
    std::env::remove_var("WATCHER_MAX_QUERY_HOURS");

    assert_eq!(status, StatusCode::OK);
    let arr = svcs.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(
        arr[0]["spans"], 1,
        "only the span inside the max-lookback ceiling should be counted"
    );
}

// JEF-546: same clamp as JEF-532's query_traces, extended to /api/logs — an
// explicit `from` far beyond the max-lookback ceiling must not defeat it, and
// with no `from` at all a full scan (e.g. an ILIKE search for a rare/absent
// term) must not walk the whole retention window either.
#[tokio::test]
#[serial]
async fn logs_from_beyond_max_lookback_is_clamped() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let day = 86_400.0;
    // Lower the ceiling to 2 days so the test doesn't need to insert 7 days of
    // data; #[serial] keeps this process-global env change from racing other
    // tests.
    std::env::set_var("WATCHER_MAX_QUERY_HOURS", "48");
    insert_log_at(&pool, "recent", 3600.0).await; // 1h ago
    insert_log_at(&pool, "old", 10.0 * day).await; // 10 days ago
    let router = app(pool);

    // Ask for a window starting 30 days ago — far past the 48h ceiling.
    let from = (chrono::Utc::now() - chrono::Duration::days(30))
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let (status, logs) = get_json(&router, &format!("/api/logs?from={from}")).await;
    std::env::remove_var("WATCHER_MAX_QUERY_HOURS");

    assert_eq!(status, StatusCode::OK);
    let services: Vec<_> = logs
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["service"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        services,
        vec!["recent"],
        "the 10-day-old log is outside the max-lookback ceiling and must stay \
         excluded even though `from` asked for 30 days back"
    );
}

#[tokio::test]
#[serial]
async fn logs_attribute_filter() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    sqlx::query(
        "INSERT INTO logs (time, service, body, attributes) VALUES
            (now(),'svc','a','{\"k8s.pod.name\":\"api-1\"}'),
            (now(),'svc','b','{\"k8s.pod.name\":\"api-2\"}')",
    )
    .execute(&pool)
    .await
    .unwrap();
    let router = app(pool);

    let (status, logs) = get_json(&router, "/api/logs?attr=k8s.pod.name=api-1").await;
    assert_eq!(status, StatusCode::OK);
    let arr = logs.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["body"], "a");

    // No attr filter → both.
    let (_, all) = get_json(&router, "/api/logs").await;
    assert_eq!(all.as_array().unwrap().len(), 2);
}

// --- Origin-side Cloudflare Access JWT verification (JEF-473) ---------------
//
// The middleware guards the UI shell + /api when Access is configured, and never
// guards /v1 ingest or /healthz. These prove the route policy end-to-end using a
// locally-signed JWT and a local JWKS server (no Cloudflare, no network).

const ACCESS_KID: &str = "test-key-1";
const ACCESS_N: &str = "uvr8rE8LT_sYjwq02YqlXZNFbHga1O3uxDiBLr7J39ELOGtLeTtl6QZF4NNJEufj_nQso32EPIffObihmofqAxiiU_JctOt0IH_Cfbbn5aVnQidUhtzo7URe_neZ4fT8lqtUyPHBcKt1Vt2p9igpntQH0hrfUAnCMXiCh9te0bgBjtV4NjtBlwhZGD8rohumJcMN8Q12gHJNsmhRIym5hvQMeth7nuff7u4Ttr6kAZ90TU57PSgUOT12pbx-UT2yiyaJUv6xMSjIhc4og-wGNRJZ7R-1Zb3WVj5Je_6bvwrA6hwgo7nxXSmKsbmoXpaWBkpxJq0uUNqG09a5-Ivhiw";
const ACCESS_E: &str = "AQAB";
const ACCESS_PEM: &str = "-----BEGIN PRIVATE KEY-----
MIIEvgIBADANBgkqhkiG9w0BAQEFAASCBKgwggSkAgEAAoIBAQC6+vysTwtP+xiP
CrTZiqVdk0VseBrU7e7EOIEuvsnf0Qs4a0t5O2XpBkXg00kS5+P+dCyjfYQ8h985
uKGah+oDGKJT8ly063Qgf8J9tuflpWdCJ1SG3OjtRF7+d5nh9PyWq1TI8cFwq3VW
3an2KCme1AfSGt9QCcIxeIKH217RuAGO1Xg2O0GXCFkYPyuiG6Ylww3xDXaAck2y
aFEjKbmG9Ax62Hue59/u7hO2vqQBn3RNTns9KBQ5PXalvH5RPbKLJolS/rExKMiF
ziiD7AY1ElntH7VlvdZWPkl7/pu/CsDqHCCjufFdKYqxuahelpYGSnEmrS5Q2obT
1rn4i+GLAgMBAAECggEABbkk/sk0mXAgIlC7lGUQBrs5RsauW5Ik2tC384xXdYha
hZGTL9THm8hbXzRYakG60tEPhLmU0J2AEa47FBXQ7eNVJKioeckzNsNyWpK8qmTT
skyt46rjXk/XcIaMqUPsb1gzMitkNmSpJM2IJEa6b2giDSZRa4vA6+66YBow3s5r
pq9WxymTWinSmrkPjiH0dQ4X5O9B8VK88ITZZHEbHmN6g7RXnVoVknGyCgV19jEr
k+iUz5Gmth9PD6tiA7fIhcKcAEyXwCRaWe1dfdWynm2gfHgjhmDME+B3BsiL14DU
lZ/VuCLcgGky+SdxqG9OsYgoBFygsiGC2DSab+Ja8QKBgQDi/w1IYDoibbaPDO+n
pm5SAa3vv7snlEcDocNUROcgUGN5qr9Kek4Me4Cgtx/RYdaf/P3SODLV47V7yI4x
dAQZlqPaKd4ogwJTdr+Bh97if+8gOYprhmZRqsIomCZrNe1f7gjP2GLyIa8FnfD2
GERliRxE3hHj9SSGfWGRXx7hfQKBgQDS3wiTDuBi1PE2cvp8rytW0dOoJKsEk0K8
wpHbRIumzhkCMwUdh2o8B46RaFSNPqaoRpiLZJykM5UZ4LKNTEcYHrwkc9W+cOF7
JhCDWpdrKDeHBnRdNEs1dhJYoIRkmNp/C1YD+PPie9X46Jib1F0FP04ihMJ2KWdD
wXimXhw9pwKBgQDWCtQWjA4lSrja+MK+nhPmpgjCSlOKxamUxiLuQi6CbOrv3c6U
xvDzmj02zpZlFFGR+LfKUw20XAxUFU/nV9NJ4Z7NZ69BGg/GbfG0jU7g2uu7wiZA
r7Gpjk+YgaewbmBPlZ+fhRX/5T0pGb4N/+H2sCwE0DWkcxKm8nFe54ex7QKBgQDF
dDD8OwbjpI/Fs35X+FK1tj7iCIvW+emZBPw8/H9kD0Kdq5aTovRYB595Ct95bvvx
QEGg7PI8U0y/cYbgBlff/w+fdpPkAqEwhmEaDl8Q6RStq96UU95Ezi25rXyrEfIu
2jeN+rSsE9c1ft8/s2fy/Oc2LWhF6tkWOfi2mBMLqwKBgBsBQelfPVHUCmI1Qsyr
hiyFRxn1pJdezc0RjETBfNsDKTJfuEcAxI4X9Z1iTgGLiJrbO5FuNhRN5ShfnrdU
rF7q76B1WCT69QpyJ+OHl0Sp6Uegf09l3QeJE6eTxDS9r7qGFPoAn5v5zBpsrrhF
lsCu2w3KPmCX769JvIYnwU8y
-----END PRIVATE KEY-----";

const ACCESS_ISSUER: &str = "https://team.cloudflareaccess.com";
const ACCESS_AUD: &str = "smoke-aud-tag";

/// Spawn a local JWKS endpoint serving the test signing key, returning its URL.
async fn spawn_jwks() -> String {
    use axum::{routing::get, Json, Router};
    let jwks = serde_json::json!({
        "keys": [{
            "kid": ACCESS_KID, "kty": "RSA", "alg": "RS256", "use": "sig",
            "n": ACCESS_N, "e": ACCESS_E,
        }]
    });
    let app = Router::new().route("/certs", get(move || async move { Json(jwks) }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}/certs")
}

/// Sign an Access-shaped JWT with the test key. `iss`/`aud`/`exp` are overridable
/// so the negative cases can produce a well-signed-but-invalid token.
fn access_token(iss: &str, aud: &str, exp_offset: i64) -> String {
    use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
    let now = now_nanos() / 1_000_000_000;
    let claims = json!({
        "iss": iss,
        "aud": [aud],
        "sub": "smoke-user",
        "email": "smoke@example.com",
        "exp": (now as i64 + exp_offset),
        "iat": now,
    });
    let mut header = Header::new(Algorithm::RS256);
    header.kid = Some(ACCESS_KID.to_string());
    let key = EncodingKey::from_rsa_pem(ACCESS_PEM.as_bytes()).unwrap();
    encode(&header, &claims, &key).unwrap()
}

/// GET a path, optionally attaching the Cf-Access-Jwt-Assertion header.
async fn get_status_with_token(
    router: &axum::Router,
    uri: &str,
    token: Option<&str>,
) -> StatusCode {
    let mut builder = Request::builder().uri(uri);
    if let Some(t) = token {
        builder = builder.header("Cf-Access-Jwt-Assertion", t);
    }
    router
        .clone()
        .oneshot(builder.body(Body::empty()).unwrap())
        .await
        .unwrap()
        .status()
}

#[tokio::test]
#[serial]
async fn access_configured_gates_api_and_ui_but_not_ingest_or_health() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let certs_url = spawn_jwks().await;
    let verifier = std::sync::Arc::new(access_jwt::Verifier::new(
        ACCESS_ISSUER,
        certs_url,
        ACCESS_AUD,
    ));
    let router = app_with_access(pool, Some(verifier));

    // /api with no token → 401.
    assert_eq!(
        get_status_with_token(&router, "/api/traces", None).await,
        StatusCode::UNAUTHORIZED,
        "/api must reject a missing Access token"
    );

    // /api with a valid token → passes (200, real JSON).
    let good = access_token(ACCESS_ISSUER, ACCESS_AUD, 3600);
    assert_eq!(
        get_status_with_token(&router, "/api/traces", Some(&good)).await,
        StatusCode::OK,
        "/api must accept a valid Access token"
    );

    // Well-signed but wrong-audience → 401 (proves aud is checked, not just sig).
    let wrong_aud = access_token(ACCESS_ISSUER, "some-other-app", 3600);
    assert_eq!(
        get_status_with_token(&router, "/api/traces", Some(&wrong_aud)).await,
        StatusCode::UNAUTHORIZED,
        "/api must reject a token minted for a different Access app"
    );

    // Expired → 401 (proves expiry is checked).
    let expired = access_token(ACCESS_ISSUER, ACCESS_AUD, -3600);
    assert_eq!(
        get_status_with_token(&router, "/api/traces", Some(&expired)).await,
        StatusCode::UNAUTHORIZED,
        "/api must reject an expired token"
    );

    // Garbage token → 401.
    assert_eq!(
        get_status_with_token(&router, "/api/traces", Some("not-a-jwt")).await,
        StatusCode::UNAUTHORIZED,
    );

    // The UI shell (SPA fallback) is gated too — no token → 401.
    assert_eq!(
        get_status_with_token(&router, "/some/spa/route", None).await,
        StatusCode::UNAUTHORIZED,
        "the UI shell must be gated, not just /api"
    );

    // /v1 ingest is NEVER gated: a tokenless OTLP POST still succeeds.
    assert_eq!(
        post_proto(
            &router,
            "/v1/metrics",
            gauge_request("cpu.load", 0.5, now_nanos()).encode_to_vec()
        )
        .await,
        StatusCode::OK,
        "/v1 ingest must stay open — collectors carry no token"
    );

    // /healthz is NEVER gated: kubelet probes it directly with no token.
    assert_eq!(
        get_status_with_token(&router, "/healthz", None).await,
        StatusCode::OK,
        "/healthz must stay open — probes carry no token"
    );
}

#[tokio::test]
#[serial]
async fn access_unconfigured_leaves_api_open() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    // No verifier → no enforcement (local dev / non-Access deploys unchanged).
    let router = app_with_access(pool, None);
    assert_eq!(
        get_status_with_token(&router, "/api/traces", None).await,
        StatusCode::OK,
        "unconfigured Access must not gate /api"
    );
}

// --- MCP server + auth (JEF-471 / JEF-493) ---------------------------------
//
// Under Cloudflare Managed OAuth the edge resolves the MCP client's opaque OAuth
// token and forwards the origin the standard `Cf-Access-Jwt-Assertion` JWT — the
// SAME header `/api` validates (JEF-473), but minted for a DEDICATED Access app
// (its own AUD, distinct from the browser app's) and validated by the shared
// `access_jwt::Verifier`. These tests reuse the browser-auth test key/JWKS but sign
// with the MCP AUD, and prove the 401/200 matrix (fail-closed) and the fail-closed
// refusal when `/mcp` is enabled without auth configured. There is no self-served
// OAuth metadata — Cloudflare owns discovery.

const MCP_AUD: &str = "smoke-mcp-aud-tag";

/// Build the app with `/mcp` enabled and assertion auth wired to a local JWKS (no
/// Cloudflare, no network), plus a freshly-signed valid MCP assertion token.
async fn mcp_app_with_auth(pool: sqlx::PgPool) -> (axum::Router, String) {
    let certs_url = spawn_jwks().await;
    let verifier =
        std::sync::Arc::new(access_jwt::Verifier::new(ACCESS_ISSUER, certs_url, MCP_AUD));
    let auth = mcp_auth::McpAuth::new(verifier);

    // Enable the endpoint for this router only (default OFF). #[serial] keeps this
    // process-global env change from racing the other tests.
    std::env::set_var("WATCHER_MCP_ENABLED", "1");
    let router = app_with_auth(pool, None, Some(auth));
    std::env::remove_var("WATCHER_MCP_ENABLED");

    (router, access_token(ACCESS_ISSUER, MCP_AUD, 3600))
}

/// POST an `initialize` frame to `/mcp` with an optional `Cf-Access-Jwt-Assertion`
/// (the header Cloudflare's edge forwards after resolving the client's opaque OAuth
/// token), returning the status and whether the MCP transport admitted the request
/// (it sets an `mcp-session-id` header on a live session).
async fn mcp_post(router: &axum::Router, assertion: Option<&str>) -> (StatusCode, bool) {
    let mut builder = Request::builder()
        .method("POST")
        .uri("/mcp")
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream");
    if let Some(t) = assertion {
        builder = builder.header("Cf-Access-Jwt-Assertion", t);
    }
    let body = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"smoke","version":"0"}}}"#;
    let resp = router
        .clone()
        .oneshot(builder.body(Body::from(body)).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let has_session = resp.headers().contains_key("mcp-session-id");
    (status, has_session)
}

#[tokio::test]
#[serial]
async fn mcp_assertion_auth_401_matrix() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let (router, valid) = mcp_app_with_auth(pool).await;

    // Missing assertion → 401, and the transport never saw the request. By default
    // (Managed OAuth) the origin emits no WWW-Authenticate — Cloudflare owns the
    // OAuth challenge/discovery.
    let (status, session) = mcp_post(&router, None).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "missing assertion must 401"
    );
    assert!(!session, "a rejected request must not open an MCP session");

    // Garbage assertion → 401.
    assert_eq!(
        mcp_post(&router, Some("not-a-jwt")).await.0,
        StatusCode::UNAUTHORIZED,
        "garbage assertion must 401"
    );

    // Well-signed but minted for a DIFFERENT Access app (browser AUD) → 401. This is
    // the crux: an MCP assertion must be scoped to the MCP app's own AUD.
    let wrong_aud = access_token(ACCESS_ISSUER, ACCESS_AUD, 3600);
    assert_eq!(
        mcp_post(&router, Some(&wrong_aud)).await.0,
        StatusCode::UNAUTHORIZED,
        "a token for the browser Access app must not be accepted at /mcp"
    );

    // Expired MCP assertion → 401.
    let expired = access_token(ACCESS_ISSUER, MCP_AUD, -3600);
    assert_eq!(
        mcp_post(&router, Some(&expired)).await.0,
        StatusCode::UNAUTHORIZED,
        "expired assertion must 401"
    );

    // Valid MCP assertion → the guard admits it (not 401); the request reaches the
    // MCP transport. Full functional proof (handshake + tool calls) is the
    // authenticated end-to-end client test below.
    let (status, _) = mcp_post(&router, Some(&valid)).await;
    assert_ne!(
        status,
        StatusCode::UNAUTHORIZED,
        "valid assertion must not 401"
    );
    assert!(
        status.is_success() || status.is_client_error(),
        "valid assertion reaches the transport, not a 5xx: {status}"
    );
}

#[tokio::test]
#[serial]
async fn mcp_fails_closed_when_auth_unconfigured() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    // Enabled but NO auth wired: /mcp must not be served unauthenticated.
    std::env::set_var("WATCHER_MCP_ENABLED", "1");
    let router = app_with_auth(pool, None, None);
    std::env::remove_var("WATCHER_MCP_ENABLED");

    // The MCP surface is not mounted: an unauthenticated POST to /mcp is NOT met by
    // the assertion guard (which would 401) — it falls through to the SPA fallback,
    // and opens no MCP session. There is simply no MCP endpoint to reach.
    let (status, session) = mcp_post(&router, None).await;
    assert_ne!(
        status,
        StatusCode::UNAUTHORIZED,
        "fail-closed /mcp is unmounted — no assertion guard present"
    );
    assert!(!session, "fail-closed /mcp must expose no MCP session");
}

/// End-to-end MCP smoke test: enable `/mcp` with assertion auth, serve the real app
/// on an ephemeral port, and drive it with the official rmcp streamable-HTTP client
/// — list the tools and call `list_services` + `query_logs`, asserting the JSON
/// shape. A tiny front layer injects the `Cf-Access-Jwt-Assertion` header on every
/// request, standing in for Cloudflare's edge (which resolves the client's opaque
/// OAuth token into that assertion). Multi-thread runtime so the server accept-loop
/// and client run concurrently.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn mcp_lists_tools_and_calls_read_queries() {
    use rmcp::{
        model::CallToolRequestParams,
        transport::{
            streamable_http_client::StreamableHttpClientTransportConfig,
            StreamableHttpClientTransport,
        },
        ServiceExt,
    };

    let Some(pool) = pool_or_skip().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    // Seed one service (span) + one log so list_services / query_logs return data.
    insert_span_at(&pool, "checkout", "mcp-tr", "mcp-s", 2.0).await;
    insert_log_at(&pool, "checkout", 2.0).await;

    let (router, token) = mcp_app_with_auth(pool).await;

    // Stand in for Cloudflare's edge: inject the `Cf-Access-Jwt-Assertion` the origin
    // validates on every request (the edge sets it after resolving the client's opaque
    // Managed-OAuth token). The rmcp client itself sends no auth header.
    let assertion: axum::http::HeaderValue = token.parse().unwrap();
    let router = router.layer(axum::middleware::from_fn(
        move |mut req: axum::extract::Request, next: axum::middleware::Next| {
            let assertion = assertion.clone();
            async move {
                req.headers_mut()
                    .insert("Cf-Access-Jwt-Assertion", assertion);
                next.run(req).await
            }
        },
    ));

    // Serve on an ephemeral port so a real MCP client can drive the transport.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });

    let config = StreamableHttpClientTransportConfig::with_uri(format!("http://{addr}/mcp"));
    let transport = StreamableHttpClientTransport::from_config(config);
    let client = ().serve(transport).await.expect("mcp handshake");

    // Tools list: exactly the nine read tools are advertised (no write/mutate tool).
    let tools = client.list_all_tools().await.expect("list_tools");
    let names: Vec<String> = tools.iter().map(|t| t.name.to_string()).collect();
    for expected in [
        "search_traces",
        "get_trace",
        "query_logs",
        "list_services",
        "service_map",
        "query_metrics",
        "metric_series",
        "list_alerts",
        "alert_events",
    ] {
        assert!(
            names.iter().any(|n| n == expected),
            "missing tool {expected} in {names:?}"
        );
    }
    assert_eq!(names.len(), 9, "exactly the nine read tools: {names:?}");

    // list_services (→ service_red): the seeded service with its RED shape.
    let res = client
        .call_tool(CallToolRequestParams::new("list_services"))
        .await
        .expect("call list_services");
    assert_ne!(res.is_error, Some(true), "list_services errored");
    let text = res
        .content
        .first()
        .and_then(|c| c.as_text())
        .expect("text content");
    let services: serde_json::Value = serde_json::from_str(&text.text).expect("services JSON");
    let svc = services
        .as_array()
        .expect("services array")
        .iter()
        .find(|s| s["service"] == "checkout")
        .expect("checkout present");
    assert!(svc["spans"].as_i64().unwrap() >= 1);

    // query_logs (→ list_logs): the seeded log with its typed fields, no filters.
    let res = client
        .call_tool(CallToolRequestParams::new("query_logs"))
        .await
        .expect("call query_logs");
    assert_ne!(res.is_error, Some(true), "query_logs errored");
    let text = res
        .content
        .first()
        .and_then(|c| c.as_text())
        .expect("text content");
    let logs: serde_json::Value = serde_json::from_str(&text.text).expect("logs JSON");
    let larr = logs.as_array().expect("logs array");
    assert_eq!(larr.len(), 1);
    assert_eq!(larr[0]["service"], "checkout");

    client.cancel().await.ok();
    server.abort();
}

// Regression for JEF-494: a faceted gauge whose latest meta row is a rollup with a
// NULL `kind` (metric_series_rollups.kind is nullable) must return 200, not 500. The
// meta query used to decode `kind` as a bare String, so "unexpected null" surfaced as
// an unlogged 500 — breaking the chart page for e.g. k8s.container.restarts.
#[tokio::test]
#[serial]
async fn facet_gauge_with_null_kind_rollup_returns_200() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let attrs = r#"{"k8s.node.name":"cluster-node-0","k8s.pod.name":"kube-proxy-abc","k8s.container.name":"kube-proxy","container.id":"deadbeef","k8s.pod.uid":"11111111-2222-3333-4444-555555555555"}"#;
    // No raw `metrics` row (pruned past retention) — only rollups, and their kind is
    // NULL, exactly the shape that 500'd in production.
    sqlx::query(
        "INSERT INTO metric_series_rollups
             (bucket, name, series_key, attrs, kind, unit, is_monotonic, count, sum, min, max, avg)
         VALUES
             (now() - interval '10 min', 'k8s.container.restarts', 'sk1', $1::jsonb, NULL, '{restart}', NULL, 1, 3, 3, 3, 3),
             (now() - interval '5 min',  'k8s.container.restarts', 'sk1', $1::jsonb, NULL, '{restart}', NULL, 1, 3, 3, 3, 3)",
    )
    .bind(attrs)
    .execute(&pool)
    .await
    .unwrap();

    let router = app(pool);
    let (status, body) = get_json(&router, "/api/metrics/facet?name=k8s.container.restarts").await;
    assert_eq!(
        status,
        StatusCode::OK,
        "facet must not 500 on a null-kind rollup"
    );
    // kind is reported as null (unknown), and the single series' points come through.
    assert!(body["kind"].is_null());
    assert_eq!(body["rated"], false);
    let series = body["series"].as_array().expect("series array");
    assert_eq!(series.len(), 1);
    assert_eq!(series[0]["points"].as_array().unwrap().len(), 2);
}
