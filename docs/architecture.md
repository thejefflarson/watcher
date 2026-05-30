# Architecture

watcher is two processes — a Rust **server** and a static **UI** — over one
PostgreSQL database. See the [ADRs](adr/) for *why*; this is *how*.

## Components & ports

| Port | Proto | Who | What |
|------|-------|-----|------|
| 4318 | HTTP  | server | OTLP ingest (`/v1/{traces,logs,metrics}`) **and** JSON query API (`/api/*`) + `/healthz` |
| 4317 | gRPC  | server | OTLP ingest (Trace/Logs/Metrics services) |
| 8080 | HTTP  | ui     | nginx serving the built SPA |

In production a single Traefik IngressRoute path-splits one host: `/v1` + `/api` +
`/healthz` → server, everything else → UI ([ADR 0006](adr/0006-single-origin-ui.md)).

## Server module map (`server/src/`)

```
main.rs        env, connect, migrate, then run HTTP + gRPC + retention concurrently
lib.rs         app(pool, auth) -> axum Router; AuthConfig + bearer-token middleware
otlp.rs        OTLP decode + storage. ingest_* (HTTP) and store_* (shared) + inserts
grpc.rs        tonic Trace/Logs/Metrics services -> the same store_* functions
api.rs         query handlers: list_traces, get_trace, list_logs, list_metrics, service_map
db.rs          PgPool + sqlx::migrate!
retention.rs   hourly background prune
migrations/    0001 spans+logs, 0002 metrics (additive, embedded at build)
```

The key seam: **transports are thin, storage is shared.** Both the HTTP handlers
and the gRPC services call `otlp::store_{traces,logs,metrics}`
([ADR 0004](adr/0004-otlp-http-and-grpc.md)).

## Data flow

```
OTel SDK / Collector / Traefik
      │  OTLP (HTTP :4318 protobuf  |  gRPC :4317)
      ▼
  ingest_* / gRPC export()  ──►  store_*  ──►  INSERT (sqlx)  ──►  Postgres
                                                                     │
  UI  ──HTTP GET /api/*──►  api::*  ──►  SELECT (sqlx)  ◄────────────┘
```

## Data model

Three flat tables, one row per span / log record / metric data point, with
`service` lifted out and everything else in `attributes JSONB`
([ADR 0003](adr/0003-denormalized-jsonb-rows.md)):

- **spans** — `trace_id, span_id, parent_span_id, service, name, start_time,
  end_time, duration_ms, status_code, …`. Unique on `(trace_id, span_id)`.
- **logs** — `time, trace_id, span_id, service, severity_*, body, …`.
- **metrics** — `time, service, name, kind (gauge|sum|histogram), value, count, unit, …`.

Derived views are pure SQL: trace summaries (`GROUP BY trace_id`), the service map
(self-join `spans` on `parent_span_id`), metric sparklines (`array_agg` slice).

## Config (env)

| Var | Default | Purpose |
|-----|---------|---------|
| `DATABASE_URL` | `postgres://watcher:watcher@localhost:5432/watcher` | Postgres |
| `BIND_ADDR` | `0.0.0.0:4318` | HTTP listener |
| `GRPC_BIND_ADDR` | `0.0.0.0:4317` | gRPC listener |
| `WATCHER_RETENTION_DAYS` | `7` | prune age; `0` disables |
| `WATCHER_INGEST_TOKEN` | — | optional bearer for `/v1` + gRPC |
| `WATCHER_API_TOKEN` | — | optional bearer for `/api` |
| `RUST_LOG` | `info,watcher_server=debug,sqlx=warn` | tracing filter |
