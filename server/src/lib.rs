pub mod alerts;
pub mod api;
pub mod db;
pub mod grpc;
pub mod otlp;
pub mod retention;
pub mod rollup;

use axum::{
    http::{header, StatusCode, Uri},
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
/// Cache policy for an embedded asset. Content-hashed bundles under `assets/`
/// are immutable (new builds get new filenames), so they can cache forever. The
/// SPA shell (index.html, served for `/` and every client route) must always be
/// revalidated — otherwise a browser or CDN pins the old shell after a deploy and
/// keeps loading stale JS even though the origin has updated.
fn cache_control(path: &str) -> &'static str {
    if path.starts_with("assets/") {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    }
}

async fn ui_handler(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };

    if let Some(asset) = Ui::get(path) {
        let mime = mime_guess::from_path(path).first_or_octet_stream();
        return (
            [
                (header::CONTENT_TYPE, mime.as_ref()),
                (header::CACHE_CONTROL, cache_control(path)),
            ],
            asset.data,
        )
            .into_response();
    }
    // Unknown path: hand back the SPA shell so the client router can take over.
    match Ui::get("index.html") {
        Some(asset) => (
            [
                (header::CONTENT_TYPE, "text/html"),
                (header::CACHE_CONTROL, "no-cache"),
            ],
            asset.data,
        )
            .into_response(),
        None => (
            StatusCode::NOT_FOUND,
            "UI not built — run `npm run build` in ui/ or use a release image.",
        )
            .into_response(),
    }
}

/// Build the HTTP router. Shared by the binary and the integration tests.
///
/// The server is unauthenticated at the app layer by design — auth lives at the
/// edge (Cloudflare Access for the public read surface) and ingest is only
/// reachable in-cluster (see ADR 0013). The permissive CORS layer keeps local
/// dev (`:5173` → `:4318`) working.
pub fn app(pool: PgPool) -> Router {
    let ingest = Router::new()
        .route("/v1/traces", post(otlp::ingest_traces))
        .route("/v1/logs", post(otlp::ingest_logs))
        .route("/v1/metrics", post(otlp::ingest_metrics));

    let api = Router::new()
        .route("/api/traces", get(api::list_traces))
        .route("/api/traces/{trace_id}", get(api::get_trace))
        .route("/api/logs", get(api::list_logs))
        .route("/api/metrics", get(api::list_metrics))
        .route("/api/metrics/series", get(api::metric_series))
        .route(
            "/api/metrics/series_grouped",
            get(api::metric_series_grouped),
        )
        .route("/api/metrics/dims", get(api::metric_dims))
        .route("/api/metrics/facet", get(api::metric_facet))
        .route("/api/metrics/histogram", get(api::metric_histogram))
        .route("/api/metrics/hist_facet", get(api::metric_hist_facet))
        .route("/api/servicemap", get(api::service_map))
        .route("/api/services", get(api::service_red))
        .route("/api/alerts", get(api::list_alerts).post(api::create_alert))
        .route("/api/alerts/events", get(api::list_alert_events))
        .route("/api/alerts/{id}", delete(api::delete_alert))
        // Span each query-API request (INFO so it's recorded) for watcher's own
        // self-telemetry. Deliberately NOT on /v1, so exporting traces to self
        // can't create a feedback loop.
        .layer(
            tower_http::trace::TraceLayer::new_for_http().make_span_with(
                tower_http::trace::DefaultMakeSpan::new().level(tracing::Level::INFO),
            ),
        );

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
