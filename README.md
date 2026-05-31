# watcher

A small, **Postgres-native** OpenTelemetry backend for **traces, logs, and metrics**,
with a built-in UI — a SigNoz-style experience **without ClickHouse**, light enough to
run on a Raspberry Pi.

- **Rust** ingest + query server (axum + sqlx), OTLP over **HTTP (:4318) and gRPC (:4317)**
- **TypeScript** UI (Vite + React): traces & waterfall, log search, metric charts, service map, alerts —
  **embedded into the server binary**, so it's one image on one port (no nginx)
- **Postgres** for storage (Timescale optional) — nothing else
- Optional bearer-token auth, background retention, metric rollups, threshold alerting

## Architecture

```
  OTel SDKs / Collector
        │  OTLP/HTTP (protobuf) :4318  ·  OTLP/gRPC :4317
        ▼
  ┌───────────────────────────────┐   POST /v1/{traces,logs,metrics}   ┌──────────────┐
  │        watcher-server         │ ─────────────────────────────────▶ │              │
  │        (Rust, axum)           │            query (sqlx)            │   Postgres   │
  │  /v1 ingest · /api query      │ ◀───────────────────────────────── │ spans/logs/  │
  │  embedded React SPA (/)       │                                    │ metrics/...  │
  └───────────────────────────────┘                                    └──────────────┘
        ▲  browser: one origin (API + UI)
        │  GET / → SPA · GET /api/... → JSON
```

- **Ingest**: OTLP on `:4318` (`/v1/traces`, `/v1/logs`, `/v1/metrics`) and gRPC
  `:4317` — a drop-in `OTEL_EXPORTER_OTLP_ENDPOINT`.
- **Store**: `spans`, `logs`, `metrics` (+ `metric_rollups`, `alert_rules/events`)
  tables, attributes as `JSONB`. Migrations run on startup.
- **Query + UI**: `/api/*` returns JSON; everything else serves the embedded SPA.

## Quick start (local)

```sh
docker compose up -d                                            # Postgres on :5432

cd server                                                      # ingest + API on :4318
DATABASE_URL=postgres://watcher:watcher@localhost:5432/watcher cargo run

cd ../ui && npm install && npm run dev                          # UI on :5173
```

Then point any OpenTelemetry SDK/Collector at `http://localhost:4318`:

```sh
OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4318 \
OTEL_TRACES_EXPORTER=otlp OTEL_LOGS_EXPORTER=otlp  your-app
```

## Deploy (Kubernetes / Helm)

The chart deploys the server (which serves both the API and the embedded UI) and a
dedicated Postgres, behind one host.

**Requirements in-cluster:** the Zalando [postgres-operator](https://github.com/zalando/postgres-operator)
(provisions the DB) and [Traefik](https://traefik.io/) (the IngressRoute). Nodes must be
able to pull the multi-arch images (ARM64 for Raspberry Pi — CI builds them).

```sh
helm upgrade --install watcher ./chart \
  --namespace watcher --create-namespace \
  --set hostname=watcher.example.com
```

The server reads its DB password from the operator-generated credential secret
(`watcher.watcher-db.credentials.postgresql.acid.zalan.do`) and composes `DATABASE_URL`
at runtime. See `chart/values.yaml` for the knobs.

## Build the image

One image — the build (context = repo root) builds the UI and embeds it in the
server binary:

```sh
docker build -f server/Dockerfile -t ghcr.io/thejefflarson/watcher-server .
```

CI (`.github/workflows/ci.yml`) builds and pushes a multi-arch (`amd64` + `arm64`)
image to GHCR on every push to `main`, alongside `cargo fmt/build/test`, the UI build,
and `helm lint`.

## Layout

```
server/      Rust ingest + query server, migrations, Dockerfile (builds + embeds the UI)
ui/          Vite + React + TS UI (built to ui/dist, embedded into the server)
chart/       Helm chart (server, Zalando Postgres, Traefik IngressRoute)
docker-compose.yml   local Postgres
```

## Docs

- [docs/architecture.md](docs/architecture.md) — components, module map, data flow, config
- [docs/adr/](docs/adr/) — architecture decision records (the *why*)
- [CONTRIBUTING.md](CONTRIBUTING.md) — dev workflow + conventions

## Roadmap

- [x] Metrics (third pillar)
- [x] OTLP/gRPC (`:4317`)
- [x] End-to-end ingest→query integration tests
- [x] Auth on ingest + UI (optional bearer tokens)
- [x] Retention (background prune; `WATCHER_RETENTION_DAYS`)
- [x] Service map
- [x] Downsampling / rollups for old data
- [x] Metric time-series charts in the UI
- [x] Alerting (threshold rules, events, optional webhook)
- [x] Single image — UI embedded in the server binary (no nginx)
- [ ] Sustained-condition alerts (`for: 5m`) and webhook retries

## License

MIT
