# 0019. Authenticate `/mcp` via Cloudflare Access Managed OAuth (validate `Cf-Access-Jwt-Assertion`)

- Status: Accepted
- Date: 2026-07-22
- Related: [0013](0013-auth-at-the-edge.md) (edge auth + origin verify), [0018](0018-read-only-mcp-server-in-process.md) (the read-only MCP server)
- Revises: the initial design recorded in this ADR (raw-`Bearer` JWT validation + self-served OAuth metadata), superseded by the spike finding below.

## Context

[0018](0018-read-only-mcp-server-in-process.md) mounted a read-only MCP server on
`/mcp`, default OFF, deliberately **unauthenticated** — its auth was left to a
follow-up. `/mcp` sits *outside* the browser edge auth that fronts the UI + `/api`
(Cloudflare Access, [0013](0013-auth-at-the-edge.md)): an MCP client (Claude Code, MCP
Inspector, claude.ai's remote connector) is not a browser and carries no Access
cookie, so the cookie-based edge policy can't gate it. Until it had its own auth the
endpoint could not be safely enabled.

The MCP authorization spec models an MCP server as an OAuth 2.0 **protected resource**:
the client obtains a token from an authorization server and presents it; the resource
server validates it. The open question was *who is the authorization server* and *what
does the origin actually receive*.

**Initial design (now revised).** The first cut had watcher itself act as the
OAuth-aware resource server: it validated the raw `Authorization: Bearer <token>` as a
Cloudflare Access **OIDC** JWT and *self-served* the RFC 9728 protected-resource
metadata (`/.well-known/oauth-protected-resource`) pointing clients at the Access OIDC
authorization server.

**Spike finding.** The mechanism Cloudflare actually provides for this is
Access **Managed OAuth**: Cloudflare is the OAuth authorization server the client needs
(including the dynamic client registration — DCR — that claude.ai's connector performs),
it issues the client an **opaque** access token, resolves that token at its **edge**,
and forwards the origin the standard **`Cf-Access-Jwt-Assertion`** JWT — the *same*
header, issuer, and team JWKS that `/api` already validates (ADR 0013). Under this model
the initial design above is wrong in two ways: the origin would receive an *opaque* token in
`Authorization: Bearer` (not a JWT — it would fail JWT validation), and OAuth
discovery/metadata is owned by Cloudflare, not the origin.

## Decision

- **Validate the forwarded `Cf-Access-Jwt-Assertion`, not the raw `Authorization`
  header.** `/mcp`'s guard validates the `Cf-Access-Jwt-Assertion` JWT the Cloudflare
  edge sets after resolving the client's opaque Managed-OAuth token — via the shared
  [`access_jwt::Verifier`](../../server/src/access_jwt.rs) (RS256 via the team's JWKS,
  `iss` = team domain, `aud`, and expiry). This is the **same** assertion model as
  ADR 0013's `/api` `access_guard`; the header-extraction + verify step is factored into
  one shared `check_access_assertion` helper both guards call. The origin only ever
  **validates**, never mints (the [0013](0013-auth-at-the-edge.md) invariant), and never
  parses the opaque OAuth token.

- **A separate AUD for the MCP app.** `/mcp` expects its **own** Access application AUD
  (`WATCHER_MCP_ACCESS_AUD`), distinct from the browser app's `WATCHER_ACCESS_AUD`, so a
  browser-scoped assertion can't be replayed at `/mcp` and vice versa. The team domain
  (`WATCHER_ACCESS_TEAM_DOMAIN`) is shared; `Verifier::for_team` derives the issuer/JWKS
  URLs for both apps.

- **No self-served OAuth metadata.** watcher no longer serves
  `/.well-known/oauth-protected-resource` (or authorization-server discovery) —
  Cloudflare Managed OAuth owns discovery, DCR, and token issuance. A
  missing/invalid/expired/wrong-AUD assertion still yields `401`. Whether the origin
  should additionally emit a `WWW-Authenticate` challenge is uncertain: under Managed
  OAuth the edge fronts the origin, so a client reaching the origin has already cleared
  the edge, and it is unknown whether Cloudflare intercepts an origin 401 challenge or
  passes it through. We therefore keep a **small, easily-toggled** path: a bare
  `WWW-Authenticate: Bearer` header, emitted only when `WATCHER_MCP_WWW_AUTHENTICATE=1`,
  **default OFF** (Cloudflare-owns-it). A live Claude-connector test will settle it;
  flipping the toggle needs no redeploy logic change.

- **Fail closed — unlike the browser guard.** The `/api` origin guard is
  *defense-in-depth* behind the edge, so on a cold-cache JWKS outage it fails **open**
  (the edge stays the gate — [0013](0013-auth-at-the-edge.md)). `/mcp`'s guard is the
  *only* auth on that surface, so it fails **closed**: an unverifiable assertion (JWKS
  unavailable) is rejected `401`, same as an invalid one. And when `WATCHER_MCP_ENABLED`
  is set but auth is unconfigured (no team domain / MCP AUD), `/mcp` is **not mounted at
  all** rather than served open — the operator gets a loud startup error. There is no
  code path that exposes an unauthenticated `/mcp`.

- **DNS-rebinding guard: the edge-set assertion replaces the Host allow-list by
  default.** rmcp's transport defaults to a loopback-only `Host` allow-list (a
  DNS-rebinding guard for locally-run servers reached by a browser). watcher's MCP is a
  server-to-server endpoint reached through a public tunnel host whose name varies by
  deployment, so that default rejects every legitimate client (why
  [0018](0018-read-only-mcp-server-in-process.md) disabled it). With assertion auth in
  front, DNS rebinding is already defeated — `Cf-Access-Jwt-Assertion` is an **edge-set**
  header (Cloudflare strips any client-supplied copy), so a rebinding attacker's browser
  JS cannot forge one regardless of `Host`. We keep the list disabled by default but let
  an operator re-scope it via `WATCHER_MCP_ALLOWED_HOSTS` for belt-and-braces. Origin
  validation stays off (MCP clients are not browsers and send no `Origin`).

## Consequences

- `/mcp` can be safely enabled: it is inert unless an operator sets
  `WATCHER_MCP_ENABLED` **and** configures `WATCHER_ACCESS_TEAM_DOMAIN` +
  `WATCHER_MCP_ACCESS_AUD` (and, at the edge, a dedicated Access application with
  **Managed OAuth** enabled). The ordering mirrors the "create the Access app first"
  runbook rule of [0013](0013-auth-at-the-edge.md).
- The 401 matrix (missing / garbage / wrong-AUD / expired → 401; valid → admitted) and
  the fail-closed refusal are covered by integration tests (`server/tests/smoke.rs`)
  using a locally-signed JWK and a local JWKS server — the edge is simulated by injecting
  the `Cf-Access-Jwt-Assertion` header. No Cloudflare, no network.
- **DECISION NEEDED / spike (human, not code — out of scope here).** The end-to-end
  question — does claude.ai's remote-connector OAuth flow **complete** against Cloudflare
  Access Managed OAuth (including DCR), and does Cloudflare intercept or forward an origin
  `401`/`WWW-Authenticate` challenge? — needs the live Access Managed-OAuth app + the
  claude.ai connector, a prod/human verification. The server side is agnostic to *how*
  the client obtained its token (it validates only the resulting edge-forwarded
  assertion), and the `WATCHER_MCP_WWW_AUTHENTICATE` toggle lets us react to the challenge
  finding without a code change. The cluster-side Access Managed-OAuth application is a
  separate GitOps/human follow-up in the `../cluster` repo (not this one).
