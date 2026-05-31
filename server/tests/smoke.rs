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
        metric, number_data_point, Gauge, Metric, NumberDataPoint, ResourceMetrics, ScopeMetrics,
    },
    resource::v1::Resource,
    trace::v1::{ResourceSpans, ScopeSpans, Span},
};
use prost::Message;
use serial_test::serial;
use std::time::{SystemTime, UNIX_EPOCH};
use tower::ServiceExt;
use watcher_server::{alerts, app, db};

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
    }
}

async fn pool_or_skip() -> Option<sqlx::PgPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let pool = db::connect(&url).await.expect("connect");
    db::migrate(&pool).await.expect("migrate");
    sqlx::query("TRUNCATE spans, logs, metrics, metric_rollups, alert_rules, alert_events")
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
                    start_time_unix_nano: 1_000_000_000,
                    end_time_unix_nano: 1_050_000_000,
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
                    time_unix_nano: 1_000_000_000,
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

    // No rollups yet, so the series comes straight from raw points.
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
async fn alert_rule_fires_and_crud() {
    let Some(pool) = pool_or_skip().await else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let router = app(pool.clone());

    // Create a rule: fire when avg(cpu.load) over the last hour exceeds 0.5.
    let body = serde_json::json!({
        "name": "cpu hot",
        "metric": "cpu.load",
        "comparator": "gt",
        "threshold": 0.5,
        "agg": "avg",
        "window_secs": 3600,
    });
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/alerts")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Invalid comparator is rejected.
    let bad = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/alerts")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({"name":"x","metric":"y","comparator":"eq","threshold":1})
                        .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(bad.status(), StatusCode::BAD_REQUEST);

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
    let rule_id = rules[0]["id"].as_i64().unwrap();

    // The firing transition is recorded as an open event.
    let (_, events) = get_json(&router, "/api/alerts/events").await;
    let events = events.as_array().expect("array");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["value"], 0.9);
    assert!(events[0]["resolved_at"].is_null());

    // Delete the rule; the list goes empty.
    let del = router
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/api/alerts/{rule_id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(del.status(), StatusCode::NO_CONTENT);
    let (_, rules) = get_json(&router, "/api/alerts").await;
    assert_eq!(rules.as_array().unwrap().len(), 0);
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
        "INSERT INTO metric_rollups (bucket, name, kind, count, sum, min, max, avg)
         VALUES (now() - make_interval(secs => $1), $2, 'gauge', 1, 1, 1, 1, 1)",
    )
    .bind(secs_ago)
    .bind(name)
    .execute(pool)
    .await
    .unwrap();
}

async fn count(pool: &sqlx::PgPool, table: &str) -> i64 {
    sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
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
    assert_eq!(reqs["points"], 2);
}

// --- Rollups ---------------------------------------------------------------

#[tokio::test]
#[serial]
async fn rollup_aggregates_completed_bucket() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    // Three points 30 minutes ago land in the same completed 5-min bucket.
    insert_metric_at(&pool, "lat", Some("api"), 10.0, 1800.0).await;
    insert_metric_at(&pool, "lat", Some("api"), 20.0, 1800.0).await;
    insert_metric_at(&pool, "lat", Some("api"), 30.0, 1800.0).await;

    let wrote = watcher_server::rollup::rollup_once(&pool, 300)
        .await
        .unwrap();
    assert!(wrote >= 1);

    let row: (i64, f64, f64, f64, f64) =
        sqlx::query_as("SELECT count, sum, min, max, avg FROM metric_rollups WHERE name = 'lat'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(row.0, 3); // count
    assert_eq!(row.1, 60.0); // sum
    assert_eq!(row.2, 10.0); // min
    assert_eq!(row.3, 30.0); // max
    assert_eq!(row.4, 20.0); // avg
}

#[tokio::test]
#[serial]
async fn rollup_is_idempotent_and_skips_current_bucket() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    insert_metric_at(&pool, "lat", Some("api"), 10.0, 1800.0).await;
    insert_metric_at(&pool, "now_pt", Some("api"), 99.0, 0.0).await; // current bucket

    watcher_server::rollup::rollup_once(&pool, 300)
        .await
        .unwrap();
    watcher_server::rollup::rollup_once(&pool, 300)
        .await
        .unwrap(); // re-run

    // The 30-min-old point rolls up to exactly one bucket (upsert, not dup).
    assert_eq!(count(&pool, "metric_rollups WHERE name = 'lat'").await, 1);
    // The current (still-filling) bucket is not rolled up yet.
    assert_eq!(
        count(&pool, "metric_rollups WHERE name = 'now_pt'").await,
        0
    );
}

#[tokio::test]
#[serial]
async fn series_stitches_rollups_and_recent_raw_without_double_count() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    // Old point → rolled up; recent point → stays raw.
    insert_metric_at(&pool, "lat", Some("api"), 10.0, 1800.0).await;
    watcher_server::rollup::rollup_once(&pool, 300)
        .await
        .unwrap();
    insert_metric_at(&pool, "lat", Some("api"), 20.0, 0.0).await;

    let router = app(pool);
    let (status, series) = get_json(&router, "/api/metrics/series?name=lat&hours=2").await;
    assert_eq!(status, StatusCode::OK);
    let arr = series.as_array().unwrap();
    // One rollup bucket (10) + one recent raw bucket (20), no overlap double-count.
    assert_eq!(arr.len(), 2, "series = {arr:?}");
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
    // spans/logs: old gone, recent kept (window = 7d).
    insert_span_at(&pool, "svc", "old", "o", 10.0 * day).await;
    insert_span_at(&pool, "svc", "new", "n", 1.0 * day).await;
    insert_log_at(&pool, "svc", 10.0 * day).await;
    insert_log_at(&pool, "svc", 1.0 * day).await;
    // raw metrics: window = 2d, so the 3-day-old point goes, the 1-day stays.
    insert_metric_at(&pool, "m", None, 1.0, 3.0 * day).await;
    insert_metric_at(&pool, "m", None, 1.0, 1.0 * day).await;
    // rollups: window = 7d, so the 10-day rollup goes, the 3-day stays.
    insert_rollup_at(&pool, "m", 10.0 * day).await;
    insert_rollup_at(&pool, "m", 3.0 * day).await;

    let deleted = watcher_server::retention::prune_once(&pool, 7, 2)
        .await
        .unwrap();
    assert!(deleted >= 4);

    assert_eq!(count(&pool, "spans").await, 1);
    assert_eq!(count(&pool, "logs").await, 1);
    assert_eq!(count(&pool, "metrics").await, 1);
    assert_eq!(count(&pool, "metric_rollups").await, 1);
}

// --- Alerts ----------------------------------------------------------------

async fn create_rule(router: &axum::Router, body: serde_json::Value) -> StatusCode {
    router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/alerts")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap()
        .status()
}

#[tokio::test]
#[serial]
async fn alert_fires_then_resolves() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let router = app(pool.clone());
    assert_eq!(
        create_rule(
            &router,
            serde_json::json!({"name":"hot","metric":"t","comparator":"gt","threshold":50,"window_secs":3600})
        )
        .await,
        StatusCode::OK
    );

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

    // lt rule fires when the value drops below the floor.
    create_rule(
        &router,
        serde_json::json!({"name":"cold","metric":"temp","comparator":"lt","threshold":0,"window_secs":3600}),
    )
    .await;
    // max-agg rule: avg would be 55 (< 80) but max is 100 (> 80) → fires.
    create_rule(
        &router,
        serde_json::json!({"name":"spike","metric":"q","comparator":"gt","threshold":80,"agg":"max","window_secs":3600}),
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
    let router = app(pool.clone());
    create_rule(
        &router,
        serde_json::json!({"name":"x","metric":"t","comparator":"gt","threshold":1,"window_secs":3600}),
    )
    .await;
    insert_metric_at(&pool, "t", None, 9.0, 1.0).await;
    alerts::evaluate_once(&pool, None).await.unwrap();
    alerts::evaluate_once(&pool, None).await.unwrap(); // still breaching
    assert_eq!(count(&pool, "alert_events").await, 1); // one open event only
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
async fn alert_rejects_bad_agg() {
    let Some(pool) = pool_or_skip().await else {
        return;
    };
    let router = app(pool);
    assert_eq!(
        create_rule(
            &router,
            serde_json::json!({"name":"x","metric":"t","comparator":"gt","threshold":1,"agg":"median"})
        )
        .await,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        create_rule(
            &router,
            serde_json::json!({"name":"","metric":"t","comparator":"gt","threshold":1})
        )
        .await,
        StatusCode::BAD_REQUEST
    );
}
