//! Cloudflare Access OIDC auth for the `/mcp` endpoint (JEF-472).
//!
//! `/mcp` (the read-only MCP server, JEF-471/ADR 0018) mounts *outside* the browser
//! edge auth that fronts the UI/`/api`: an MCP client is not a browser and carries no
//! Access cookie. Instead it authenticates with an `Authorization: Bearer <token>`
//! whose token is a Cloudflare Access-issued JWT minted for a **dedicated** Access
//! application (its own AUD, distinct from the browser app's). The origin only ever
//! **validates** that token — it never mints one (ADR 0013) — reusing the shared
//! [`access_jwt::Verifier`] (RS256 + JWKS + `iss`/`aud`/`exp`).
//!
//! ## Fail **closed** — unlike the browser guard
//!
//! The `/api` guard ([`crate::access_guard`]) is *defense-in-depth* behind the edge,
//! so on a cold-cache JWKS outage it fails **open** (the edge stays the gate). `/mcp`
//! is the **primary** and only auth on that surface, so it fails **closed**: a missing,
//! invalid, expired, wrong-audience *or* unverifiable (JWKS-unavailable) token is
//! rejected `401`. It must never admit an unauthenticated client.
//!
//! ## Discovery (MCP auth spec, 2025-06-18 / RFC 9728)
//!
//! A `401` carries a `WWW-Authenticate: Bearer` challenge pointing at
//! `/.well-known/oauth-protected-resource`, whose JSON names the Cloudflare Access
//! OIDC authorization server. That lets a spec-compliant MCP client discover where to
//! obtain a token. The metadata document is served **unauthenticated** (the client
//! fetches it *before* it has a token).

use std::sync::Arc;

use axum::{
    body::Body,
    http::{header, HeaderMap, Request, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;

use crate::access_jwt::{Verifier, VerifyError};

/// The Access application AUD tag for the MCP app. Distinct from the browser app's
/// `WATCHER_ACCESS_AUD` so a browser-scoped token can't be replayed at `/mcp`.
const MCP_AUD_ENV: &str = "WATCHER_MCP_ACCESS_AUD";

/// The Cloudflare Access team domain, shared with the browser guard (JEF-473).
const TEAM_DOMAIN_ENV: &str = "WATCHER_ACCESS_TEAM_DOMAIN";

/// Path of the OAuth Protected Resource Metadata document (RFC 9728 §3.1, with the
/// resource's `/mcp` path suffixed as the spec prescribes for a non-root resource).
pub const METADATA_PATH: &str = "/.well-known/oauth-protected-resource/mcp";

/// A root-level alias for the same document. Some clients probe the un-suffixed path.
pub const METADATA_PATH_ROOT: &str = "/.well-known/oauth-protected-resource";

/// Auth context for `/mcp`: the token verifier plus the authorization-server URL
/// advertised to clients. Built once and shared (cheap `Arc` clone) across the guard
/// middleware and the metadata handler.
pub struct McpAuth {
    verifier: Arc<Verifier>,
    /// The Cloudflare Access OIDC authorization server (the team domain issuer).
    authorization_server: String,
}

impl McpAuth {
    /// Construct from an explicit verifier (used by tests pointing at a local JWKS).
    /// The authorization server advertised in metadata is the verifier's issuer.
    pub fn new(verifier: Arc<Verifier>) -> Self {
        let authorization_server = verifier.issuer().to_string();
        Self {
            verifier,
            authorization_server,
        }
    }

    /// Build from the environment, or `None` when MCP auth is not configured.
    ///
    /// Requires both `WATCHER_ACCESS_TEAM_DOMAIN` (shared with the browser guard) and
    /// `WATCHER_MCP_ACCESS_AUD` (the MCP Access app's own AUD). When either is unset,
    /// returns `None` — and the caller then refuses to serve `/mcp` at all (fail
    /// closed: never expose an unauthenticated MCP surface).
    pub fn from_env() -> Option<Self> {
        let team = env_nonempty(TEAM_DOMAIN_ENV)?;
        let aud = env_nonempty(MCP_AUD_ENV)?;
        Some(Self::new(Arc::new(Verifier::for_team(&team, aud))))
    }
}

fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// axum middleware: require a valid Access Bearer token on `/mcp`, else `401` with a
/// resource-metadata discovery challenge. Fails **closed** (see the module docs).
pub async fn bearer_guard(auth: Arc<McpAuth>, req: Request<Body>, next: Next) -> Response {
    let token = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(bearer_token);

    let Some(token) = token else {
        tracing::warn!("rejecting MCP request: missing/!Bearer Authorization header");
        return challenge(
            req.headers(),
            "a Cloudflare Access Bearer token is required",
        );
    };

    match auth.verifier.verify(token).await {
        Ok(_claims) => next.run(req).await,
        // MCP is the primary auth (not defense-in-depth), so an unresolvable JWKS
        // must fail CLOSED — never admit an unverified client on a certs outage.
        Err(e @ VerifyError::KeysUnavailable) => {
            tracing::warn!("rejecting MCP request (fail closed): {e}");
            challenge(req.headers(), "token could not be verified")
        }
        Err(e @ VerifyError::Invalid(_)) => {
            tracing::warn!("rejecting MCP request: {e}");
            challenge(req.headers(), "the Bearer token is invalid or expired")
        }
    }
}

/// Handler for the OAuth Protected Resource Metadata document (RFC 9728). Points the
/// client at the Cloudflare Access OIDC authorization server. Served unauthenticated.
pub async fn protected_resource_metadata(
    auth: Arc<McpAuth>,
    headers: HeaderMap,
) -> Json<serde_json::Value> {
    let base = base_url(&headers);
    Json(json!({
        "resource": format!("{base}/mcp"),
        "authorization_servers": [auth.authorization_server],
        "bearer_methods_supported": ["header"],
    }))
}

/// Extract the token from an `Authorization: Bearer <token>` value (scheme match is
/// case-insensitive per RFC 7235), or `None` when it isn't a non-empty Bearer.
fn bearer_token(value: &str) -> Option<&str> {
    let (scheme, token) = value.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let token = token.trim();
    (!token.is_empty()).then_some(token)
}

/// A `401` carrying the `WWW-Authenticate: Bearer` discovery challenge, pointing the
/// client at the resource-metadata document (derived from the request's host, so it's
/// correct behind whatever tunnel host fronts the pod). `detail` is a fixed,
/// code-supplied string (never caller input), so it can't inject header bytes.
fn challenge(headers: &HeaderMap, detail: &str) -> Response {
    let metadata_url = format!("{}{METADATA_PATH}", base_url(headers));
    let www = format!(
        "Bearer error=\"invalid_token\", error_description=\"{detail}\", \
         resource_metadata=\"{metadata_url}\""
    );
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, www)],
        "unauthorized",
    )
        .into_response()
}

/// Reconstruct the request's origin (`scheme://host`) from headers. Behind Cloudflare
/// the origin sees the public host via `Host` and the scheme via `X-Forwarded-Proto`.
/// The host is sanitized to a conservative charset so a hostile `Host` can't smuggle
/// junk into the metadata/challenge we echo back; anything odd falls back to a
/// placeholder rather than being reflected.
fn base_url(headers: &HeaderMap) -> String {
    let host = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .filter(|h| is_sane_host(h))
        .unwrap_or("localhost");
    let scheme = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next())
        .map(str::trim)
        .filter(|s| *s == "http" || *s == "https")
        .unwrap_or("https");
    format!("{scheme}://{host}")
}

/// A permissive but safe host check: DNS labels, IPv6 literals, and an optional port.
fn is_sane_host(host: &str) -> bool {
    !host.is_empty()
        && host.len() <= 255
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b':' | b'[' | b']'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bearer_token_parses_case_insensitively() {
        assert_eq!(bearer_token("Bearer abc"), Some("abc"));
        assert_eq!(bearer_token("bearer  abc "), Some("abc"));
        assert_eq!(bearer_token("Basic abc"), None);
        assert_eq!(bearer_token("Bearer "), None);
        assert_eq!(bearer_token("abc"), None);
    }

    #[test]
    fn base_url_uses_host_and_forwarded_proto() {
        let mut h = HeaderMap::new();
        h.insert(header::HOST, "watcher.example.com".parse().unwrap());
        assert_eq!(base_url(&h), "https://watcher.example.com");
        h.insert("x-forwarded-proto", "http".parse().unwrap());
        assert_eq!(base_url(&h), "http://watcher.example.com");
    }

    #[test]
    fn base_url_rejects_hostile_host_and_proto() {
        let mut h = HeaderMap::new();
        // A header value can't carry CRLF (hyper rejects it), but odd bytes still get
        // scrubbed to the placeholder rather than reflected into the response.
        h.insert(header::HOST, "evil host/with space".parse().unwrap());
        h.insert("x-forwarded-proto", "javascript".parse().unwrap());
        assert_eq!(base_url(&h), "https://localhost");
    }
}
