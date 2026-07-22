# 0013. Authenticate at the edge, then verify at the origin

- Status: Accepted
- Date: 2026-05-30
- Amended: 2026-07-21 (JEF-473 — add origin-side Access JWT verification)
- Supersedes: [0008](0008-optional-bearer-auth.md)

## Context

[0008](0008-optional-bearer-auth.md) gave watcher optional in-app bearer tokens. In
practice they were a single shared secret with no identity, no MFA, no revocation,
and a clunky UX (the UI prompted for a token and stored it in `localStorage`). They
also didn't protect the UI shell — only `/api`. Once we decided to expose the UI
through a Cloudflare tunnel, a proper edge auth story (Cloudflare Access) made the
app-layer tokens redundant.

## Decision

Remove app-layer auth entirely. The server is unauthenticated; protection comes from
where the traffic does:

- **Public read surface (UI + `/api`):** Cloudflare Access (SSO + MFA, per-user,
  revocable, audited) gates the hostname at the edge — including the UI shell. The
  SPA's same-origin `/api` calls ride the Access cookie.
- **Ingest (`/v1` HTTP + gRPC):** never exposed publicly. The IngressRoute carves
  out `/v1` ([ADR 0011](0011-metric-rollups.md) era chart), and gRPC isn't routed at
  all; senders reach the server via the in-cluster Service.

`AuthConfig`, the bearer middleware, the gRPC interceptor, the UI token control, and
the chart's token Secret are all gone.

## Consequences

- Real identity on the read path and a write path with no public surface — strictly
  stronger than the shared token it replaces, with less code.
- watcher now *depends* on the edge for public auth: if you expose the host without
  an Access policy in front, it's open. The deployment runbook makes "create the
  Access app first" the required order.
- For environments without Cloudflare, equivalent edge auth (Traefik forwardAuth +
  an SSO proxy) is the substitute; the app deliberately holds no auth of its own.

## Amendment — origin-side Access JWT verification (JEF-473, 2026-07-21)

Edge-only auth trusts that every request to `/api` and the UI arrived through Access.
An Access-policy slip, a tunnel/ingress misconfig, or direct in-cluster access to the
pod bypasses that trust and reaches the read surface unauthenticated. As
defense-in-depth the origin now **verifies** the Access-issued JWT itself — it still
mints nothing.

- **What.** An axum middleware ([`access_jwt`](../../server/src/access_jwt.rs) +
  `app_with_access` in `server/src/lib.rs`) validates the `Cf-Access-Jwt-Assertion`
  header against Access's JWKS: RS256 signature, `iss` (team domain), `aud` (the Access
  application's AUD tag), and expiry. A missing or invalid token is rejected `401`. The
  verifier is transport-agnostic (token + expected `iss`/`aud` in, claims out), so the
  planned `/mcp` Bearer-token guard reuses it.
- **Route policy.** Enforced on the UI shell + `/api` only. `/v1` OTLP ingest and
  `/healthz` are **never** gated — in-cluster collectors and kubelet probes carry no
  token, and gating them would break ingest and readiness.
- **Configuration & fail-open.** Enabled only when both `WATCHER_ACCESS_TEAM_DOMAIN`
  and `WATCHER_ACCESS_AUD` are set; unset means no enforcement, so local dev and
  non-Access deployments are unaffected (still edge-gated, as before). The JWKS is
  cached and refreshed hourly. If a refresh fails but keys were cached, the last-known
  keys are served; if keys have **never** been fetched (cold cache) and the certs
  endpoint is unreachable, the middleware **fails open** with a loud warning rather
  than hard-failing every request — the edge remains the primary gate, and a transient
  Cloudflare certs outage must not take the whole read surface down. This is the
  correct trade for a secondary, defense-in-depth layer.
- **Consequence.** The origin no longer blindly trusts edge routing for `/api` + UI.
  The failure mode is availability-biased by design (documented above); a deployment
  wanting a strictly-closed origin would need a different policy for the cold-cache
  case, which we can revisit if the threat model changes.
