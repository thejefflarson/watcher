pub mod access_jwt;
pub mod alerts;
pub mod api;
pub mod db;
pub mod grpc;
pub mod mcp;
pub mod mcp_auth;
pub mod online_ddl;
pub mod otlp;
pub mod retention;
pub mod selflog;
pub mod selfmon;
pub mod selftrace;

use std::sync::Arc;

use axum::{
    body::Body,
    extract::State,
    http::{header, Request, StatusCode, Uri},
    middleware::Next,
    response::{IntoResponse, Response},
    routing::{get, post},
    Router,
};
use rust_embed::RustEmbed;
use sqlx::PgPool;
use tower_http::cors::CorsLayer;
use tracing_opentelemetry::OpenTelemetrySpanExt;

use crate::access_jwt::{Verifier, VerifyError};
use crate::mcp_auth::McpAuth;

/// The header Cloudflare Access sets on requests that cleared its edge policy,
/// carrying the signed identity JWT the origin re-verifies (ADR 0013). Under
/// Managed OAuth this is also what the edge forwards after resolving an MCP
/// client's opaque OAuth token (ADR 0019), so `/api` and `/mcp` verify the same
/// header. Cloudflare strips any client-supplied `Cf-Access-*` header, so the
/// origin can trust it as edge-set.
const ACCESS_JWT_HEADER: &str = "Cf-Access-Jwt-Assertion";

/// Outcome of checking a request's `Cf-Access-Jwt-Assertion` header against a
/// [`Verifier`]. Shared by the `/api` [`access_guard`] (which fails **open** on
/// `KeysUnavailable`, since the edge is still the gate) and the `/mcp`
/// [`mcp_auth::assertion_guard`] (which fails **closed** — it is the only auth).
pub(crate) enum Assertion {
    /// A valid assertion — admit the request.
    Valid,
    /// No `Cf-Access-Jwt-Assertion` header on the request.
    Missing,
    /// Header present but the JWT did not validate (bad signature, wrong
    /// `aud`/`iss`, or expired); carries the reason for the guard's warn log.
    Invalid(String),
    /// The JWKS could not be obtained (cold cache) so the token can't be checked;
    /// the caller decides fail-open vs. fail-closed.
    KeysUnavailable,
}

/// Verify a request's `Cf-Access-Jwt-Assertion` header against `verifier`. The
/// single place the header name and the `VerifyError` → outcome mapping live, so
/// the `/api` and `/mcp` guards share exactly one verification path (ADR 0019).
pub(crate) async fn check_access_assertion(
    verifier: &Verifier,
    headers: &axum::http::HeaderMap,
) -> Assertion {
    let Some(token) = headers.get(ACCESS_JWT_HEADER).and_then(|v| v.to_str().ok()) else {
        return Assertion::Missing;
    };
    match verifier.verify(token).await {
        Ok(_claims) => Assertion::Valid,
        Err(VerifyError::KeysUnavailable) => Assertion::KeysUnavailable,
        Err(VerifyError::Invalid(why)) => Assertion::Invalid(why),
    }
}

/// Auth state for the read surface (UI shell + `/api`), carried by [`access_guard`].
///
/// Two independent verifiers back the one guard (ADR 0013 + ADR 0020):
/// - `edge`: the Cloudflare Access assertion on `Cf-Access-Jwt-Assertion`
///   (defense-in-depth behind the edge; fails **open** on a JWKS outage).
/// - `internal`: a Kubernetes ServiceAccount-token JWT on `Authorization: Bearer`,
///   for in-cluster callers (the fixer-dashboard) that never traverse Cloudflare
///   Access. A verified internal token is admitted **only** when its `sub` is in
///   `allowed_subs`; it fails **closed** (a cluster-JWKS outage admits no internal
///   caller — the request carries no edge header and so hits `Missing` → 401).
///
/// Cheaply cloneable (both verifiers behind `Arc`, the allowlist shared), as axum's
/// `from_fn_with_state` clones the state per request.
#[derive(Clone, Default)]
pub struct ReadAuth {
    /// Cloudflare Access verifier, or `None` when edge verification is unconfigured.
    pub edge: Option<Arc<Verifier>>,
    /// Internal Kubernetes SA-token verifier, or `None` when internal auth is off.
    pub internal: Option<Arc<Verifier>>,
    /// The exact `sub` claims an internal token may carry, e.g.
    /// `system:serviceaccount:fixer-dashboard:fixer-dashboard`. Empty disables the
    /// internal path (enforced in the binary, which refuses to wire an allowlist-less
    /// internal verifier — fail closed).
    pub allowed_subs: Arc<Vec<String>>,
}

impl ReadAuth {
    /// Whether any origin-side verification is configured. When neither verifier is
    /// set the guard is not wired at all (local dev / non-Access deploys stay open).
    fn is_enabled(&self) -> bool {
        self.edge.is_some() || self.internal.is_some()
    }
}

/// Reads W3C trace-context headers off an incoming request so watcher can
/// continue the caller's trace (e.g. traefik's) rather than starting a new one.
struct HeaderExtractor<'a>(&'a axum::http::HeaderMap);
impl opentelemetry::propagation::Extractor for HeaderExtractor<'_> {
    fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).and_then(|v| v.to_str().ok())
    }
    fn keys(&self) -> Vec<&str> {
        self.0.keys().map(|k| k.as_str()).collect()
    }
}

/// Span for one `/api` request, parented to the upstream `traceparent` context
/// so traefik → watcher → (DB) lands in a single trace instead of a fresh root.
fn otel_request_span(req: &Request<Body>) -> tracing::Span {
    let span = tracing::info_span!(
        "http.request",
        otel.name = tracing::field::Empty,
        http.method = %req.method(),
        http.route = %req.uri().path(),
    );
    span.record(
        "otel.name",
        format!("{} {}", req.method(), req.uri().path()).as_str(),
    );
    let parent = opentelemetry::global::get_text_map_propagator(|p| {
        p.extract(&HeaderExtractor(req.headers()))
    });
    // set_parent returns a Result in tracing-opentelemetry 0.33; a failed
    // parent link on our own self-telemetry span is non-fatal, so ignore it.
    let _ = span.set_parent(parent);
    span
}

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

/// Pull a bearer credential off `Authorization: Bearer <token>`, if present.
fn bearer_token(headers: &axum::http::HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|t| !t.is_empty())
}

/// Middleware that guards the read surface (UI shell + `/api`) at the origin
/// (ADR 0013 + ADR 0020). Two paths, tried in order:
///
/// 1. **Internal** (ADR 0020): if an internal verifier is configured and the request
///    carries `Authorization: Bearer <SA-token>`, verify it. A valid token with an
///    allowlisted `sub` is admitted; a valid token with a non-allowlisted `sub` is a
///    hard `401` (a presented-but-unauthorized identity is a reject, not a fall-through).
///    A token that fails to verify (bad/expired/unverifiable) **falls through** to the
///    edge check — the internal path never fails *open*, so an internal caller (which
///    sends no edge header) then hits `Missing` below and is rejected.
/// 2. **Edge** (ADR 0013): the Cloudflare Access assertion on `Cf-Access-Jwt-Assertion`,
///    unchanged — `Valid`/`KeysUnavailable` admit (edge fail-open preserved),
///    `Missing`/`Invalid` reject `401`.
///
/// Wired **only** onto the UI shell + `/api` (never `/v1` ingest or `/healthz`) and
/// only when at least one verifier is configured — see [`app_with_auth`]. Token
/// contents are never logged, only the `sub`.
async fn access_guard(State(auth): State<ReadAuth>, req: Request<Body>, next: Next) -> Response {
    // 1. Internal Kubernetes SA-token path (ADR 0020).
    if let Some(internal) = &auth.internal {
        if let Some(token) = bearer_token(req.headers()) {
            match internal.verify(token).await {
                Ok(claims) => {
                    if auth.allowed_subs.iter().any(|s| s == &claims.sub) {
                        return next.run(req).await;
                    }
                    tracing::warn!(
                        sub = %claims.sub,
                        path = %req.uri().path(),
                        "rejecting internal token: sub not in allowlist",
                    );
                    return (StatusCode::UNAUTHORIZED, "internal token not authorized")
                        .into_response();
                }
                // Invalid / unverifiable: do NOT fail open here — fall through to the
                // edge check, where an internal caller (no edge header) hits `Missing`.
                Err(why) => {
                    tracing::debug!(
                        path = %req.uri().path(),
                        "internal bearer did not verify ({why}); falling through to edge auth",
                    );
                }
            }
        }
    }

    // 2. Cloudflare Access edge path (ADR 0013), unchanged.
    let Some(edge) = &auth.edge else {
        // No edge verifier: an internal-only deployment where no valid internal token
        // was presented. Fail closed.
        tracing::warn!(
            path = %req.uri().path(),
            "rejecting request: no valid internal token and edge auth not configured",
        );
        return (StatusCode::UNAUTHORIZED, "missing or invalid token").into_response();
    };
    match check_access_assertion(edge.as_ref(), req.headers()).await {
        Assertion::Valid => next.run(req).await,
        // Cold-cache / JWKS-outage: fail open (the edge is still the gate) so a
        // Cloudflare certs blip can't take the whole read surface down.
        Assertion::KeysUnavailable => next.run(req).await,
        Assertion::Missing => {
            tracing::warn!(
                path = %req.uri().path(),
                "rejecting request with no {ACCESS_JWT_HEADER} header",
            );
            (StatusCode::UNAUTHORIZED, "missing Cloudflare Access token").into_response()
        }
        Assertion::Invalid(why) => {
            tracing::warn!(path = %req.uri().path(), "rejecting request: invalid Access token: {why}");
            (StatusCode::UNAUTHORIZED, "invalid Cloudflare Access token").into_response()
        }
    }
}

/// Build the HTTP router with no origin-side auth (MCP auth, if any, still comes from
/// the environment). Shared by the binary and the integration tests; the binary calls
/// [`app_with_access`] when Access is configured. See ADR 0013 for the edge-auth
/// design this augments.
pub fn app(pool: PgPool) -> Router {
    app_with_auth(pool, ReadAuth::default(), McpAuth::from_env())
}

/// Build the HTTP router, optionally enforcing origin-side read-surface auth
/// (Cloudflare Access, ADR 0013, and/or internal SA-token, ADR 0020); MCP auth is
/// taken from the environment.
pub fn app_with_access(pool: PgPool, read_auth: ReadAuth) -> Router {
    app_with_auth(pool, read_auth, McpAuth::from_env())
}

/// Build the HTTP router, optionally enforcing Cloudflare Access JWT verification
/// (ADR 0013) on the read surface and Managed-OAuth assertion auth (ADR 0019) on `/mcp`.
///
/// The server holds no app-layer auth by default — auth lives at the edge
/// (Cloudflare Access for the public read surface) and ingest is only reachable
/// in-cluster (ADR 0013). When `read_auth` configures an edge and/or internal verifier
/// the UI shell + `/api` are additionally guarded at the origin (see [`ReadAuth`] and
/// [`access_guard`]); `/v1` ingest and `/healthz` are **never** gated (in-cluster
/// collectors and kubelet probes carry no token). The permissive CORS layer keeps
/// local dev (`:5173` → `:4318`) working.
///
/// `/mcp` (when `WATCHER_MCP_ENABLED`) is served **only** when `mcp_auth` is `Some`:
/// with no auth configured it is refused rather than exposed unauthenticated (fail
/// closed). Its guard validates the same `Cf-Access-Jwt-Assertion` the edge forwards
/// after resolving the MCP client's opaque Managed-OAuth token — but with a distinct
/// AUD and failing closed. It is wired outside the browser Access guard since an MCP
/// client is not a browser.
pub fn app_with_auth(pool: PgPool, read_auth: ReadAuth, mcp_auth: Option<McpAuth>) -> Router {
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
        .route("/api/metrics/exemplars", get(api::metric_exemplars))
        .route("/api/servicemap", get(api::service_map))
        .route("/api/services", get(api::service_red))
        // Rules are declarative (reconciled from config at startup), so this is
        // read-only — no create/delete routes.
        .route("/api/alerts", get(api::list_alerts))
        .route("/api/alerts/events", get(api::list_alert_events))
        // Span each query-API request (INFO so it's recorded) for watcher's own
        // self-telemetry. Deliberately NOT on /v1, so exporting traces to self
        // can't create a feedback loop.
        .layer(tower_http::trace::TraceLayer::new_for_http().make_span_with(otel_request_span));

    // The guarded surface: `/api` plus the SPA fallback (the UI shell). When any
    // origin-side verifier is configured, the JWT middleware wraps exactly these — not
    // ingest, healthz, or /mcp.
    let guarded = api.fallback(ui_handler);
    let guarded = if read_auth.is_enabled() {
        guarded.layer(axum::middleware::from_fn_with_state(
            read_auth,
            access_guard,
        ))
    } else {
        guarded
    };

    let mut router = Router::new()
        // Never gated: kubelet hits /healthz and in-cluster collectors hit /v1
        // directly, neither carrying an Access token.
        .route("/healthz", get(api::healthz))
        .merge(ingest)
        .merge(guarded);

    // Read-only MCP server (ADR 0018), opt-in via WATCHER_MCP_ENABLED (default OFF).
    // Nested as its own tower service *outside* the `/api` router — and therefore
    // outside the browser Access guard above, since an MCP client is not a browser
    // and carries no Access cookie. Under Cloudflare Managed OAuth (ADR 0019) the edge
    // resolves the client's opaque OAuth token and forwards a `Cf-Access-Jwt-Assertion`;
    // `mcp_auth::assertion_guard` validates that assertion (its own AUD, fail-closed).
    // Cloudflare owns OAuth discovery, so no `.well-known` metadata is self-served.
    //
    // Fail closed: when MCP is enabled but no auth is configured, `/mcp` is not
    // mounted at all (the operator-facing error is logged in `main`) — we never
    // expose an unauthenticated MCP surface.
    if mcp::enabled() {
        if let Some(auth) = mcp_auth {
            // The MCP transport behind the assertion guard. Nesting before
            // `with_state` keeps the service off `with_state` (it carries its own pool).
            let auth = Arc::new(auth);
            let guarded_mcp = Router::new()
                .nest_service("/mcp", mcp::service(pool.clone()))
                .layer(axum::middleware::from_fn(move |req, next| {
                    mcp_auth::assertion_guard(auth.clone(), req, next)
                }));
            router = router.merge(guarded_mcp);
        }
    }

    router.layer(CorsLayer::permissive()).with_state(pool)
}
