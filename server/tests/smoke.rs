//! End-to-end ingest → query test. Requires a Postgres reachable via DATABASE_URL
//! (CI provides one as a service container); skips cleanly if it's unset.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use opentelemetry_proto::tonic::{
    collector::logs::v1::ExportLogsServiceRequest,
    collector::trace::v1::ExportTraceServiceRequest,
    common::v1::{any_value, AnyValue, KeyValue},
    logs::v1::{LogRecord, ResourceLogs, ScopeLogs},
    resource::v1::Resource,
    trace::v1::{ResourceSpans, ScopeSpans, Span},
};
use prost::Message;
use tower::ServiceExt;
use watcher_server::{app, db};

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
    sqlx::query("TRUNCATE spans, logs")
        .execute(&pool)
        .await
        .expect("truncate");
    Some(pool)
}

#[tokio::test]
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
