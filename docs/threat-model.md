# watcher — Threat Model

_Last reviewed: 2026-06-27. Companion to [`architecture.md`](architecture.md) and
the ADRs (notably [0004 OTLP HTTP+gRPC](adr/0004-otlp-http-and-grpc.md),
[0008 optional bearer auth](adr/0008-optional-bearer-auth.md),
[0013 auth at the edge](adr/0013-auth-at-the-edge.md))._

## 1. What this is

watcher is a Postgres-native OpenTelemetry traces + logs + metrics backend with
an embedded React UI. A single Rust (axum + sqlx) process ingests OTLP, stores
signals in Postgres, serves a query API + SPA, and evaluates threshold alert
rules that fire email/webhook notifications.

## 2. Deployment & trust boundaries

watcher runs **inside a k3s cluster**. This is the single most important fact for
reasoning about its exposure — the attacker model is _not_ "anyone on the
internet."

```
            internet
               │
       ┌───────▼─────────┐   Access-gated Cloudflare tunnel
       │ Cloudflare Access│  (authenticates a human identity)
       └───────┬─────────┘
               │  only the READ surface (SPA + /api) is published
   ╔═══════════▼══════════════════════════════════════════╗
   ║  k3s cluster (trust boundary)                         ║
   ║                                                       ║
   ║   collectors ──OTLP──▶ watcher :4318 (HTTP) / :4317  ║  ← in-cluster only,
   ║                         :4318 also serves /api + SPA  ║    NOT tunnel-exposed
   ║   watcher ──▶ Postgres (Zalando operator)             ║
   ║   watcher ──▶ SMTP relay / webhook (alerts)           ║
   ╚═══════════════════════════════════════════════════════╝
```

- **OTLP ingest (`:4318` HTTP, `:4317` gRPC)** is reachable **only from inside the
  cluster**. It is unauthenticated by design ([ADR 0013](adr/0013-auth-at-the-edge.md)):
  the assumption is that the cluster network — ideally narrowed by a NetworkPolicy
  — is the boundary, not an app-layer token.
- **Read surface (SPA + `/api`)** is the only thing published, through an
  **Access-gated Cloudflare tunnel**. Cloudflare Access authenticates a human
  identity at the edge; the server itself enforces nothing.
- **No app-layer authentication or authorization** in the server
  ([ADR 0013](adr/0013-auth-at-the-edge.md); an optional bearer token was
  considered and deferred in [ADR 0008](adr/0008-optional-bearer-auth.md)).

### Reaching the unauthenticated surfaces therefore requires one of:

1. **An in-cluster foothold** — a compromised or malicious neighbor pod with
   network reach to `:4318`/`:4317` (for ingest attacks), or
2. **A valid Cloudflare Access identity** (in a single-tenant homelab, the
   operator) or a **tunnel/ingress misconfiguration** (for read-API attacks).

There is no anonymous-internet path to any watcher endpoint.

## 3. Inputs

**Trusted** (maintainer/operator-controlled):

- Rust/TS source, embedded SQL migrations, embedded SPA assets.
- Environment config: `DATABASE_URL`, `BIND_ADDR`/`GRPC_BIND_ADDR`,
  `WATCHER_RETENTION_*`, `WATCHER_ALERT_*` (SMTP host/port/from/to, webhook URL).
- The declarative alert-rules JSON (`WATCHER_ALERTS_CONFIG`, rendered from the
  chart's `server.alerts`) — enum-whitelisted before reconcile.
- DB password and SMTP credentials from k8s secrets.

**Untrusted**:

- **OTLP ingest payloads** (protobuf over HTTP/gRPC) from any in-cluster sender.
  Resource/span/log/metric attributes are stored as JSONB and later rendered in
  the UI. Timestamps come from attacker-supplied `time_unix_nano`.
- **Query-API parameters** on `/api/*` (service, name, `attr` key=value,
  `errors_only`, `min_duration_ms`, `from`/`to`, `limit`, `group_by`, path
  params) — flow into runtime sqlx queries.
- The metric **values** an alert reads (they originate from untrusted ingest),
  though alert _definitions_ are trusted config.

## 4. Controls in place

- **Network position** is the primary control for ingest: in-cluster only, no
  tunnel exposure. (Verify the cluster's NetworkPolicy actually scopes
  `:4318`/`:4317` to the real collectors — that is the load-bearing control.)
- **Cloudflare Access** gates the read surface at the edge.
- **SQL is fully parameterized** — every query uses sqlx `.bind()`; the one
  `format!`-built fragment (`agg_expr` in alert evaluation) maps through a
  hardcoded enum whitelist, and retention's `format!` uses only a hardcoded
  identifier list. No injection path found.
- **React default escaping** — all stored telemetry is rendered as escaped JSX
  text; no `dangerouslySetInnerHTML`, no URL/HTML built from attribute values,
  no prototype-pollution sink. Static assets are served from an in-binary
  `rust-embed` map (no filesystem path traversal).
- **Ingest size bounds** — axum 2 MB body default, an explicit 64 MiB
  decompressed-gzip cap (zip-bomb guard), a per-request metric-point cap, and
  (after the 2026-06-27 review) a **recursion-depth cap on attribute nesting**
  and a **timestamp-overflow guard** (see §6).
- **DB**: 60 s `statement_timeout`; credentials never logged.
- **Alert delivery**: STARTTLS-enforced SMTP (no plaintext fallback); webhook URL
  is config-only (no SSRF from request data); rule names → email body, never SMTP
  headers.

## 5. Findings & residual risk (2026-06-27 review)

A full OWASP/LLM-Top-10 review found **no injection, XSS, SSRF, auth-bypass, or
secret-exposure issues**. The residual items are availability/robustness gaps,
**re-rated for the in-cluster + edge-gated deployment** (the raw findings assumed
an internet attacker; that path does not exist here):

| Risk | Reachability | Effective severity | Disposition |
| --- | --- | --- | --- |
| Stack-overflow crash from deeply-nested OTLP attributes | in-cluster ingest, **or an accidental malformed payload from a real collector** | Medium | **Fixed** — depth cap (§6) |
| `u64→i64` timestamp wrap → far-past stamp pruned by retention | in-cluster ingest, or a collector clock glitch | Low | **Fixed** — overflow guard (§6) |
| Shared 10-conn pool exhaustion via expensive `/api` queries (`/api/logs` unbounded `ILIKE`, `/api/metrics/dims` scan) | needs Access identity or tunnel misconfig; mostly a **self-DoS / heavy-query** risk | Low | Accepted; optional query-window floor + rate limit if it bites |
| gRPC fan-out (no per-request point cap on traces/logs), no explicit tonic limits | in-cluster ingest | Low | Accepted (defense-in-depth) |
| `CorsLayer::permissive()` on the whole surface | no `allow-credentials`, so no live cross-origin read | Low | Accepted; tighten to an allowlist if ever credentialed |
| No `nosniff`/CSP headers; Postgres `sslmode=Prefer` (in-cluster) | — | Low | Accepted (defense-in-depth) |

### Accepted risks (explicit)

- **No app-layer auth** ([ADR 0013](adr/0013-auth-at-the-edge.md)). Compromise of
  the cluster network, or a misconfigured tunnel that exposes ingest or bypasses
  Access, collapses the model. The compensating controls are the NetworkPolicy
  and the Cloudflare Access configuration — both **outside this codebase** and the
  real thing to keep correct.
- watcher is an **observability tool, not a system of record**. The worst
  in-code outcome is a watcher restart or degraded/incorrect telemetry — no RCE,
  no data exfiltration, no privilege escalation surface.

## 6. Hardening applied (2026-06-27)

- `server/src/otlp.rs` `any_value_to_json`: **recursion-depth cap (`MAX_ATTR_DEPTH
  = 32`)** on nested `ArrayValue`/`KvlistValue` — over-nested subtrees are dropped
  to `Null` instead of overflowing the stack. Closes the one-message crash
  (hostile or accidental).
- `server/src/otlp.rs` `ts`: **checked `u64→i64` conversion** — `time_unix_nano`
  past `i64::MAX` falls back to receive time instead of wrapping to ~1969 and
  being silently pruned by retention.

## 7. If the threat model changes

If ingest is ever published beyond the cluster, or app-layer auth is wanted,
revisit [ADR 0008](adr/0008-optional-bearer-auth.md) (optional bearer token) and
add rate limiting + a request-window floor on the analytic query endpoints before
exposing `/api` without an edge gate.
