# watcher

A small, **Postgres-native** OpenTelemetry **traces + logs** backend with a built-in UI
— a SigNoz-style trace/log experience **without ClickHouse**, light enough to run on a
Raspberry Pi.

- **Rust** ingest + query server (axum + sqlx)
- **TypeScript** UI (Vite + React): trace list, trace waterfall, log search
- **Postgres** for storage (Timescale optional) — nothing else

> Status: **v0**. OTLP/HTTP ingest for traces + logs, Postgres storage, query API, and
> a working UI. Metrics, OTLP/gRPC, and auth are on the roadmap.

## Architecture

```
  OTel SDKs / Collector
        │  OTLP/HTTP (protobuf)
        ▼
  ┌─────────────────┐   POST /v1/traces   ┌────────────┐
  │  watcher-server │──────────────────▶ │            │
  │  (Rust, axum)   │   POST /v1/logs     │  Postgres  │
  │                 │◀──────────────────  │ spans/logs │
  │  GET /api/...   │   query (sqlx)      └────────────┘
  └─────────────────┘
        ▲  fetch JSON
  ┌─────────────────┐
  │   watcher-ui    │  React: trace list · waterfall · log search
  └─────────────────┘
```

- **Ingest**: OTLP/HTTP on `:4318` (`/v1/traces`, `/v1/logs`) — a drop-in
  `OTEL_EXPORTER_OTLP_ENDPOINT`.
- **Store**: `spans` and `logs` tables, attributes as `JSONB`. Migrations run on startup.
- **Query**: `/api/traces`, `/api/traces/{trace_id}`, `/api/logs`.

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

The chart deploys the server, the UI, and a dedicated Postgres, behind one host.

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

## Build the images

```sh
docker build -t ghcr.io/thejefflarson/watcher-server ./server
docker build -t ghcr.io/thejefflarson/watcher-ui ./ui   # build-arg VITE_API_BASE="" (same-origin)
```

CI (`.github/workflows/ci.yml`) builds and pushes multi-arch (`amd64` + `arm64`) images
to GHCR on every push to `main`, alongside `cargo fmt/build/test`, the UI build, and
`helm lint`.

## Layout

```
server/      Rust ingest + query server, migrations, Dockerfile
ui/          Vite + React + TS UI, Dockerfile, nginx.conf
chart/       Helm chart (server, ui, Zalando Postgres, Traefik IngressRoute)
docker-compose.yml   local Postgres
```

## Roadmap

- [ ] Metrics (third pillar)
- [ ] OTLP/gRPC (`:4317`)
- [ ] End-to-end ingest→query integration test
- [ ] Auth on ingest + UI
- [ ] Retention / downsampling
- [ ] Service map

## License

MIT
