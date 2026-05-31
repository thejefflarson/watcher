# Architecture

watcher is a single Rust **server** — OTLP ingest, JSON query API, and the
embedded **UI** — over one PostgreSQL database. See the [ADRs](adr/) for *why*;
this is *how*.

## Components & ports

| Port | Proto | What |
|------|-------|------|
| 4318 | HTTP  | OTLP ingest (`/v1/{traces,logs,metrics}`), JSON query API (`/api/*`), `/healthz`, **and the SPA** (everything else) |
| 4317 | gRPC  | OTLP ingest (Trace/Logs/Metrics services) |

The built UI (`ui/dist`) is compiled into the binary with `rust-embed` and served
as the axum fallback, so there's one image, one port, one origin — no nginx, no
path-split ([ADR 0010](adr/0010-ui-embedded-in-server-binary.md)). A Traefik
IngressRoute (when enabled) routes the host to the server, but **carves out `/v1`
by default** (`ingressRoute.exposeIngest: false`) so OTLP ingest is never publicly
routable — telemetry is pushed in-cluster via the Service. Pair public exposure
with edge auth (Cloudflare Access) for the read surface.

## Server module map (`server/src/`)

```
main.rs        env, connect, migrate, then run HTTP + gRPC + retention + rollup + alerts concurrently
lib.rs         app(pool, auth) -> axum Router; AuthConfig + bearer middleware; rust-embed UI fallback
otlp.rs        OTLP decode + storage. ingest_* (HTTP) and store_* (shared) + inserts
grpc.rs        tonic Trace/Logs/Metrics services -> the same store_* functions
api.rs         query handlers: traces, logs, metrics, metric series, service map, alert CRUD
db.rs          PgPool + sqlx::migrate!
retention.rs   hourly background prune (prune_once); raw metrics on a shorter window than rollups
rollup.rs      periodic downsample of raw metrics into metric_rollups (rollup_once)
alerts.rs      periodic threshold evaluation (evaluate_once) -> alert_events + optional webhook
migrations/    0001 spans+logs, 0002 metrics, 0003 metric_rollups, 0004 alerts (additive, embedded)
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
- **metric_rollups** — pre-aggregated metric buckets (`bucket, name, service, count,
  sum, min, max, avg`) so history survives raw-point pruning ([ADR 0011](adr/0011-metric-rollups.md)).
- **alert_rules** / **alert_events** — threshold rules and their firing/resolved
  transitions ([ADR 0012](adr/0012-alerting.md)).

Derived views are pure SQL: trace summaries (`GROUP BY trace_id`), the service map
(self-join `spans` on `parent_span_id`), metric sparklines (`array_agg` slice). A
metric time series stitches `metric_rollups` (old) with raw `metrics` newer than the
last rollup bucket, so it stays continuous after pruning.

## Config (env)

| Var | Default | Purpose |
|-----|---------|---------|
| `DATABASE_URL` | `postgres://watcher:watcher@localhost:5432/watcher` | Postgres |
| `BIND_ADDR` | `0.0.0.0:4318` | HTTP listener |
| `GRPC_BIND_ADDR` | `0.0.0.0:4317` | gRPC listener |
| `WATCHER_RETENTION_DAYS` | `7` | prune age for spans/logs/rollups; `0` disables |
| `WATCHER_METRICS_RAW_DAYS` | `2` | prune age for raw metric points (rollups keep history); `0` = same as retention |
| `WATCHER_ROLLUP_BUCKET_SECS` | `300` | downsample bucket width; `0` disables rollups |
| `WATCHER_ALERT_INTERVAL_SECS` | `30` | how often alert rules are evaluated (min 5) |
| `WATCHER_ALERT_WEBHOOK` | — | optional URL to POST on alert fire/resolve |
| `RUST_LOG` | `info,watcher_server=debug,sqlx=warn` | tracing filter |

The server has **no app-layer auth** ([ADR 0013](adr/0013-auth-at-the-edge.md)): the
public read surface is gated by Cloudflare Access at the edge, and ingest is only
reachable in-cluster (`/v1` carved out of the public route, gRPC never routed).
