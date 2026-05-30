pub mod api;
pub mod db;
pub mod grpc;
pub mod otlp;
pub mod retention;

use std::sync::Arc;

use axum::{
    extract::{Request, State},
    http::StatusCode,
    middleware::{self, Next},
    response::Response,
    routing::{get, post},
    Router,
};
use sqlx::PgPool;
use tower_http::cors::CorsLayer;

/// Optional bearer tokens. When `None`, that surface is unauthenticated.
#[derive(Clone, Default)]
pub struct AuthConfig {
    pub ingest: Option<Arc<str>>,
    pub api: Option<Arc<str>>,
}

impl AuthConfig {
    pub fn from_env() -> Self {
        let read = |k: &str| {
            std::env::var(k)
                .ok()
                .filter(|s| !s.is_empty())
                .map(Arc::from)
        };
        Self {
            ingest: read("WATCHER_INGEST_TOKEN"),
            api: read("WATCHER_API_TOKEN"),
        }
    }
}

/// Middleware: if a token is configured, require `Authorization: Bearer <token>`.
async fn require_token(
    State(expected): State<Option<Arc<str>>>,
    req: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    if let Some(token) = expected {
        let ok = req
            .headers()
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .map(|t| t == &*token)
            .unwrap_or(false);
        if !ok {
            return Err(StatusCode::UNAUTHORIZED);
        }
    }
    Ok(next.run(req).await)
}

/// Build the HTTP router. Shared by the binary and the integration tests.
pub fn app(pool: PgPool, auth: AuthConfig) -> Router {
    let ingest = Router::new()
        .route("/v1/traces", post(otlp::ingest_traces))
        .route("/v1/logs", post(otlp::ingest_logs))
        .route("/v1/metrics", post(otlp::ingest_metrics))
        .route_layer(middleware::from_fn_with_state(auth.ingest, require_token));

    let api = Router::new()
        .route("/api/traces", get(api::list_traces))
        .route("/api/traces/{trace_id}", get(api::get_trace))
        .route("/api/logs", get(api::list_logs))
        .route("/api/metrics", get(api::list_metrics))
        .route("/api/servicemap", get(api::service_map))
        .route_layer(middleware::from_fn_with_state(auth.api, require_token));

    Router::new()
        .route("/healthz", get(healthz))
        .merge(ingest)
        .merge(api)
        .layer(CorsLayer::permissive())
        .with_state(pool)
}

async fn healthz() -> &'static str {
    "ok"
}
