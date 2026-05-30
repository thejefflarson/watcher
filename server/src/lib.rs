pub mod api;
pub mod db;
pub mod otlp;

use axum::{
    routing::{get, post},
    Router,
};
use sqlx::PgPool;
use tower_http::cors::CorsLayer;

/// Build the application router. Shared by the binary and the integration tests.
pub fn app(pool: PgPool) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        // OTLP/HTTP ingestion (protobuf).
        .route("/v1/traces", post(otlp::ingest_traces))
        .route("/v1/logs", post(otlp::ingest_logs))
        // Query API.
        .route("/api/traces", get(api::list_traces))
        .route("/api/traces/{trace_id}", get(api::get_trace))
        .route("/api/logs", get(api::list_logs))
        .layer(CorsLayer::permissive())
        .with_state(pool)
}

async fn healthz() -> &'static str {
    "ok"
}
