//! Cloudflare Access **Managed OAuth** auth for the `/mcp` endpoint (JEF-493).
//!
//! `/mcp` (the read-only MCP server, JEF-471/ADR 0018) mounts *outside* the browser
//! edge auth that fronts the UI/`/api`: an MCP client is not a browser and carries no
//! Access cookie. The chosen production mechanism is Cloudflare Access **Managed
//! OAuth**: Cloudflare is the OAuth authorization server the MCP client (claude.ai's
//! remote connector) registers with (DCR) and obtains an **opaque** token from;
//! Cloudflare resolves that token at its edge and forwards the origin the standard
//! **`Cf-Access-Jwt-Assertion`** JWT — the *same* header/issuer/team-JWKS that `/api`
//! validates (JEF-473). So the origin only ever **validates** that assertion — it
//! never mints one (ADR 0013), never parses the opaque `Authorization: Bearer`, and
//! never self-serves OAuth metadata (Cloudflare owns discovery).
//!
//! This supersedes JEF-472's design, which validated the raw `Authorization: Bearer`
//! as a JWT (now the *opaque* Managed-OAuth token → would be rejected) and self-served
//! `/.well-known/oauth-protected-resource`. See ADR 0019.
//!
//! ## Fail **closed** — unlike the browser guard
//!
//! The `/api` guard ([`crate::access_guard`]) is *defense-in-depth* behind the edge,
//! so on a cold-cache JWKS outage it fails **open** (the edge stays the gate). `/mcp`
//! is the **primary** and only auth on that surface, so it fails **closed**: a missing,
//! invalid, expired, wrong-audience *or* unverifiable (JWKS-unavailable) assertion is
//! rejected `401`. It must never admit an unauthenticated client.

use std::sync::Arc;

use axum::{
    body::Body,
    http::{header, Request, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};

use crate::access_jwt::Verifier;
use crate::Assertion;

/// The Access application AUD tag for the MCP app. Distinct from the browser app's
/// `WATCHER_ACCESS_AUD` so a browser-scoped assertion can't be replayed at `/mcp`.
const MCP_AUD_ENV: &str = "WATCHER_MCP_ACCESS_AUD";

/// The Cloudflare Access team domain, shared with the browser guard (JEF-473).
const TEAM_DOMAIN_ENV: &str = "WATCHER_ACCESS_TEAM_DOMAIN";

/// Opt-in toggle: emit a bare `WWW-Authenticate: Bearer` challenge on a `401`.
/// Default OFF — under Managed OAuth the Cloudflare edge owns the OAuth
/// challenge/discovery, so the origin normally stays silent (a client that reaches
/// the origin already cleared the edge). A live Claude-connector test will tell us
/// whether Cloudflare intercepts the challenge or the origin must emit it; flip this
/// on (`WATCHER_MCP_WWW_AUTHENTICATE=1`) if the origin turns out to need it.
const WWW_AUTHENTICATE_ENV: &str = "WATCHER_MCP_WWW_AUTHENTICATE";

/// Auth context for `/mcp`: the Access-assertion verifier plus the challenge toggle.
/// Built once and shared (cheap `Arc` clone) across the guard middleware.
pub struct McpAuth {
    verifier: Arc<Verifier>,
    /// When set, a `401` carries a bare `WWW-Authenticate: Bearer` challenge (see
    /// [`WWW_AUTHENTICATE_ENV`]). Default OFF: Cloudflare owns OAuth discovery.
    emit_challenge: bool,
}

impl McpAuth {
    /// Construct from an explicit verifier (used by tests pointing at a local JWKS).
    /// The `WWW-Authenticate` challenge is off — matching the production default.
    pub fn new(verifier: Arc<Verifier>) -> Self {
        Self {
            verifier,
            emit_challenge: false,
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
        Some(Self {
            verifier: Arc::new(Verifier::for_team(&team, aud)),
            emit_challenge: env_flag(WWW_AUTHENTICATE_ENV),
        })
    }

    /// A `401` for a rejected `/mcp` request, optionally carrying a bare
    /// `WWW-Authenticate: Bearer` challenge (see [`Self::emit_challenge`]).
    fn unauthorized(&self) -> Response {
        if self.emit_challenge {
            (
                StatusCode::UNAUTHORIZED,
                [(header::WWW_AUTHENTICATE, "Bearer")],
                "unauthorized",
            )
                .into_response()
        } else {
            (StatusCode::UNAUTHORIZED, "unauthorized").into_response()
        }
    }
}

fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Parse a truthy env flag (`1`/`true`/`on`); anything else (incl. unset) is false.
fn env_flag(key: &str) -> bool {
    std::env::var(key)
        .map(|v| matches!(v.trim(), "1" | "true" | "on"))
        .unwrap_or(false)
}

/// axum middleware: require a valid `Cf-Access-Jwt-Assertion` on `/mcp`, else `401`.
/// Reuses the shared [`crate::check_access_assertion`] the `/api` guard runs, but
/// fails **closed** (see the module docs): unlike `/api` it rejects even a
/// JWKS-unavailable assertion, since `/mcp` has no edge auth behind it.
pub async fn assertion_guard(auth: Arc<McpAuth>, req: Request<Body>, next: Next) -> Response {
    let detail = match crate::check_access_assertion(&auth.verifier, req.headers()).await {
        Assertion::Valid => return next.run(req).await,
        Assertion::Missing => "missing Cf-Access-Jwt-Assertion header".to_string(),
        Assertion::Invalid(why) => format!("invalid Access assertion: {why}"),
        // MCP is the primary auth (not defense-in-depth), so an unresolvable JWKS
        // must fail CLOSED — never admit an unverified client on a certs outage.
        Assertion::KeysUnavailable => {
            "Access assertion could not be verified (JWKS unavailable)".to_string()
        }
    };
    tracing::warn!("rejecting MCP request (fail closed): {detail}");
    auth.unauthorized()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn auth(emit_challenge: bool) -> McpAuth {
        McpAuth {
            verifier: Arc::new(Verifier::new(
                "https://team.cloudflareaccess.com",
                "http://127.0.0.1:1/certs",
                "aud",
            )),
            emit_challenge,
        }
    }

    #[test]
    fn unauthorized_omits_challenge_by_default() {
        let resp = auth(false).unauthorized();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        assert!(
            !resp.headers().contains_key(header::WWW_AUTHENTICATE),
            "Cloudflare owns discovery by default — origin stays silent"
        );
    }

    #[test]
    fn unauthorized_emits_bare_bearer_challenge_when_toggled() {
        let resp = auth(true).unauthorized();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            resp.headers().get(header::WWW_AUTHENTICATE).unwrap(),
            "Bearer"
        );
    }
}
