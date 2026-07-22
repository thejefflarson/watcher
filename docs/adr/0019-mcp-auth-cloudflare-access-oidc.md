# 0019. Authenticate `/mcp` with a Cloudflare Access OIDC Bearer token

- Status: Accepted
- Date: 2026-07-21
- Related: [0013](0013-auth-at-the-edge.md) (edge auth + origin verify), [0018](0018-read-only-mcp-server-in-process.md) (the read-only MCP server)

## Context

[0018](0018-read-only-mcp-server-in-process.md) mounted a read-only MCP server on
`/mcp`, default OFF, deliberately **unauthenticated** — its auth was left to this
ticket (JEF-472). `/mcp` sits *outside* the browser edge auth that fronts the UI +
`/api` (Cloudflare Access, [0013](0013-auth-at-the-edge.md)): an MCP client (Claude
Code, MCP Inspector, claude.ai's remote connector) is not a browser and carries no
Access cookie, so the cookie-based edge policy can't gate it. Until it had its own
auth the endpoint could not be safely enabled.

The MCP authorization spec (2025-06-18) models an MCP server as an OAuth 2.0
**protected resource**: the client obtains a token from an authorization server and
presents it as `Authorization: Bearer <token>`; the resource server validates it and,
on failure, points the client at the authorization server via
[RFC 9728](https://www.rfc-editor.org/rfc/rfc9728) Protected Resource Metadata.

We already verify Cloudflare Access JWTs at the origin
([`access_jwt::Verifier`](../../server/src/access_jwt.rs), JEF-473). Cloudflare Access
can also act as an **OIDC provider** for a dedicated Access application, minting JWTs
we can validate with the *same* verifier. So the edge still owns identity; watcher
only validates, never mints (the [0013](0013-auth-at-the-edge.md) invariant).

## Decision

- **Bearer validation, reusing the shared verifier.** `/mcp` requires
  `Authorization: Bearer <token>`; the token is validated by the shared
  `access_jwt::Verifier` (RS256 via the team's JWKS, `iss` = team domain, `aud`, and
  expiry). No JWT logic is duplicated — a small [`mcp_auth`](../../server/src/mcp_auth.rs)
  module wires the verifier into an axum middleware and the discovery routes.

- **A separate AUD for the MCP app.** The MCP endpoint expects its **own** Access
  application AUD (`WATCHER_MCP_ACCESS_AUD`), distinct from the browser app's
  `WATCHER_ACCESS_AUD`, so a browser-scoped token can't be replayed at `/mcp` and vice
  versa. The team domain (`WATCHER_ACCESS_TEAM_DOMAIN`) is shared. `Verifier::for_team`
  derives the issuer/JWKS URLs from that team domain for both apps.

- **401, not a redirect — with discovery.** A missing / invalid / expired / wrong-AUD
  token yields `401` with a `WWW-Authenticate: Bearer … resource_metadata="…"`
  challenge (never an HTML login redirect — the caller is a program). The
  `/.well-known/oauth-protected-resource/mcp` document (also served at the un-suffixed
  root path some clients probe) is returned **unauthenticated** and names the
  Cloudflare Access OIDC authorization server, so a spec-compliant client can discover
  where to get a token.

- **Fail closed — unlike the browser guard.** The `/api` origin guard is
  *defense-in-depth* behind the edge, so on a cold-cache JWKS outage it fails **open**
  (the edge stays the gate — [0013](0013-auth-at-the-edge.md)). `/mcp` has **no** edge
  in front of it; its Bearer guard is the *only* auth, so it fails **closed**: an
  unverifiable token (JWKS unavailable) is rejected `401`, same as an invalid one. And
  when `WATCHER_MCP_ENABLED` is set but auth is unconfigured (no team domain / MCP
  AUD), `/mcp` is **not mounted at all** rather than served open — the operator gets a
  loud startup error. There is no code path that exposes an unauthenticated `/mcp`.

- **DNS-rebinding guard: Bearer auth replaces the Host allow-list by default.** rmcp's
  transport defaults to a loopback-only `Host` allow-list (a DNS-rebinding guard for
  locally-run servers reached by a browser). watcher's MCP is a server-to-server
  endpoint reached through a public tunnel host whose name varies by deployment, so
  that default rejects every legitimate client (why [0018](0018-read-only-mcp-server-in-process.md)
  disabled it). With Bearer auth now in front, DNS rebinding is already defeated — a
  rebinding attacker's browser JS cannot forge a valid Access token, so it never gets
  past the guard regardless of `Host`. We therefore keep the list disabled by default
  but let an operator re-scope it to their known host(s) via `WATCHER_MCP_ALLOWED_HOSTS`
  (comma-separated) for belt-and-braces. Origin validation stays off (MCP clients are
  not browsers and send no `Origin`).

## Consequences

- `/mcp` can be safely enabled: it is inert unless an operator sets
  `WATCHER_MCP_ENABLED` **and** configures `WATCHER_ACCESS_TEAM_DOMAIN` +
  `WATCHER_MCP_ACCESS_AUD` (and, at the edge, a dedicated Access OIDC application). The
  ordering mirrors the "create the Access app first" runbook rule of
  [0013](0013-auth-at-the-edge.md).
- The 401/200 matrix (missing / garbage / wrong-AUD / expired → 401 with the metadata
  challenge; valid → admitted), the resource-metadata document, and the fail-closed
  refusal are covered by integration tests (`server/tests/smoke.rs`) using a
  locally-signed JWK and a local JWKS server — no Cloudflare, no network.
- **DECISION NEEDED / spike (human, not code — out of scope here).** The end-to-end
  question — does claude.ai's remote-connector OAuth flow actually **complete** against
  Cloudflare Access OIDC, including **dynamic client registration (DCR)**? — needs the
  live Access OIDC app + the claude.ai connector, which is a prod/human verification.
  The server side built here is spec-compliant (RFC 9728 protected-resource metadata +
  Bearer validation) and is agnostic to *how* the client obtained its token. If
  Cloudflare Access does **not** support DCR for the connector, the fallback is a
  **pre-registered OAuth client** (register the client with Access once, configure the
  connector with those credentials) — no server change required either way, since
  watcher only validates the resulting token. The cluster-side Access OIDC application
  is a separate GitOps/human follow-up in the `../cluster` repo (not this one).
