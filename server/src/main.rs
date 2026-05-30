mod api;
mod db;
mod otlp;

use axum::{
    routing::{get, post},
    Router,
};
use tower_http::cors::CorsLayer;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info,watcher_server=debug,sqlx=warn")),
        )
        .init();

    let database_url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://watcher:watcher@localhost:5432/watcher".to_string());
    let bind = std::env::var("BIND_ADDR").unwrap_or_else(|_| "0.0.0.0:4318".to_string());

    let pool = db::connect(&database_url).await?;
    db::migrate(&pool).await?;

    let app = Router::new()
        .route("/healthz", get(healthz))
        // OTLP/HTTP ingestion (protobuf) — drop-in OTEL_EXPORTER_OTLP_ENDPOINT target.
        .route("/v1/traces", post(otlp::ingest_traces))
        .route("/v1/logs", post(otlp::ingest_logs))
        // Query API for the UI.
        .route("/api/traces", get(api::list_traces))
        .route("/api/traces/{trace_id}", get(api::get_trace))
        .route("/api/logs", get(api::list_logs))
        .layer(CorsLayer::permissive())
        .with_state(pool);

    let listener = tokio::net::TcpListener::bind(&bind).await?;
    tracing::info!("watcher listening on http://{bind}  (OTLP/HTTP + /api)");
    axum::serve(listener, app).await?;
    Ok(())
}

async fn healthz() -> &'static str {
    "ok"
}
