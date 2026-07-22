# 0018. Expose the read API as a read-only MCP server, in-process on /mcp

- Status: Accepted
- Date: 2026-07-21
- Related: [0013](0013-auth-at-the-edge.md), [0010](0010-ui-embedded-in-server-binary.md), [0002](0002-rust-axum-sqlx-runtime-queries.md)

## Context

watcher already exposes its telemetry through a same-origin query API (`/api/...`)
consumed by the embedded UI. LLM agents (Claude Code, MCP Inspector, …) increasingly
speak the **Model Context Protocol** (MCP): given an MCP endpoint they can search
traces, read logs/metrics, and inspect services/alerts as tools. We want watcher to
be that endpoint without standing up a second process or duplicating query logic
(JEF-471).

Two shapes were possible:

1. **Official Rust MCP SDK (`rmcp`) mounted in-process** over its streamable-HTTP
   transport, nested on the existing axum app, or
2. a **hand-rolled JSON-RPC-2.0-over-HTTP** axum route implementing `initialize` /
   `tools/list` / `tools/call` ourselves.

The ticket allowed the hand-rolled path only if `rmcp`'s axum integration was not
mature.

## Decision

- **Use `rmcp` (option 1).** Its `transport-streamable-http-server` ships a
  `StreamableHttpService` that implements `tower::Service`, so it nests straight into
  the axum router with `Router::nest_service("/mcp", …)` — no second listener, no
  bespoke protocol code. It is the official SDK (`modelcontextprotocol/rust-sdk`),
  tracks the spec (session handling, SSE framing, DNS-rebinding guards), and its axum
  story is mature. The client half is a **dev-dependency only** (used by the smoke
  test), so the shipped binary carries only the server transport.

- **Tools are thin wrappers over the existing read queries.** The nine read handlers'
  query bodies were refactored in `api.rs` into shared `pub async fn query_*(pool, …)`
  functions; both the HTTP handler and the MCP tool call the *same* function, so every
  limit clamp and default time window (e.g. traces' 24h window, logs' 2000-row cap) is
  shared by construction — the MCP surface cannot introduce an unbounded scan the HTTP
  surface doesn't already permit. Tools return the exact `api` response structs as JSON.
  Exposed: `search_traces`, `get_trace`, `query_logs`, `list_services`, `service_map`,
  `query_metrics`, `metric_series`, `list_alerts`, `alert_events`.

- **Read-only, by construction.** There are no write/mutate tools; `/mcp` touches no
  ingest (`/v1`) or alert-reconcile path. (Alert rules are already declarative and the
  HTTP `/api/alerts` surface is read-only — ADR 0012.)

- **Opt-in, default OFF, unauthenticated for now.** `/mcp` mounts only when
  `WATCHER_MCP_ENABLED` is truthy. It is deliberately mounted *outside* the edge
  auth that fronts the UI/`/api` (Cloudflare Access, ADR 0013): an MCP client is not a
  browser and carries no Access cookie. Its own auth is a **separate** ticket
  (JEF-472); until that lands the endpoint must not be exposed, so it defaults off and
  the flag's doc comment says so. The transport's default loopback-only Host allow-list
  (a DNS-rebinding guard aimed at locally-run servers reached by a browser) is disabled
  here, since watcher's MCP is a server-to-server endpoint reached through a public
  tunnel host where that list would reject every legitimate client.

## Consequences

- watcher is usable directly from an MCP client with zero extra deployment — one
  binary, same origin — consistent with the embedded-UI posture (ADR 0010) and
  Postgres-only runtime queries (ADR 0002).
- Query behavior stays identical across the HTTP and MCP surfaces because they share
  the `query_*` functions; a future change to a clamp or window applies to both.
- The endpoint is inert until an operator sets `WATCHER_MCP_ENABLED` **and** (once
  JEF-472 lands) configures its auth. Enabling it before then exposes read access to
  anyone who can reach the host — the flag default and the ADR make that ordering
  explicit, mirroring the "create the Access app first" runbook rule of ADR 0013.
- `rmcp` (and, for tests, its client + `reqwest` 0.13) enters the dependency tree; the
  server transport is small and the client stays out of the release binary.
