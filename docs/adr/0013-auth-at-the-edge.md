# 0013. Authenticate at the edge, not in the app

- Status: Accepted
- Date: 2026-05-30
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
