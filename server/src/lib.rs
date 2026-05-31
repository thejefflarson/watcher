pub mod alerts;
pub mod api;
pub mod db;
pub mod grpc;
pub mod otlp;
pub mod retention;
pub mod rollup;

use std::sync::Arc;

use axum::{
    extract::{Request, State},
    http::{header, StatusCode, Uri},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
    Router,
};
use rust_embed::RustEmbed;
use sqlx::PgPool;
use tower_http::cors::CorsLayer;

/// The built UI (`ui/dist`), embedded into the binary at compile time so the
/// server is a single self-contained artifact — no nginx, no static-file
/// sidecar. `build.rs` guarantees the folder exists; in CI's server-only build
/// it's empty and the fallback serves a short notice instead.
#[derive(RustEmbed)]
#[folder = "../ui/dist"]
struct Ui;

/// Serve an embedded UI asset, falling back to `index.html` for client-side
/// routes (the SPA pattern). This is the router fallback, so it only runs for
/// paths not claimed by `/api`, `/v1`, or `/healthz`.
async fn ui_handler(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };

    if let Some(asset) = Ui::get(path) {
        let mime = mime_guess::from_path(path).first_or_octet_stream();
        return ([(header::CONTENT_TYPE, mime.as_ref())], asset.data).into_response();
    }
    // Unknown path: hand back the SPA shell so the client router can take over.
    match Ui::get("index.html") {
        Some(asset) => ([(header::CONTENT_TYPE, "text/html")], asset.data).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            "UI not built — run `npm run build` in ui/ or use a release image.",
        )
            .into_response(),
    }
}

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
        .route("/api/metrics/series", get(api::metric_series))
        .route("/api/servicemap", get(api::service_map))
        .route("/api/alerts", get(api::list_alerts).post(api::create_alert))
        .route("/api/alerts/events", get(api::list_alert_events))
        .route("/api/alerts/{id}", delete(api::delete_alert))
        .route_layer(middleware::from_fn_with_state(auth.api, require_token));

    Router::new()
        .route("/healthz", get(healthz))
        .merge(ingest)
        .merge(api)
        // Anything not an API/ingest/health route is the embedded UI (SPA).
        .fallback(ui_handler)
        .layer(CorsLayer::permissive())
        .with_state(pool)
}

async fn healthz() -> &'static str {
    "ok"
}
