# watcher

A small, **Postgres-native** OpenTelemetry **traces + logs** backend — Rust ingest/query
server, TypeScript UI. No ClickHouse.

The goal: a SigNoz-style trace/log experience that runs comfortably on a Raspberry Pi
and stores everything in plain Postgres (Timescale optional).

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
        │
  ┌─────────────────┐
  │   watcher-ui    │  Vite + React + TS: trace list, waterfall, log search
  └─────────────────┘
```

- **Ingest**: OTLP/HTTP on `:4318` (`/v1/traces`, `/v1/logs`) — a drop-in
  `OTEL_EXPORTER_OTLP_ENDPOINT`.
- **Store**: `spans` and `logs` tables, attributes as `JSONB`. Migrations run on startup.
- **Query**: `/api/traces`, `/api/traces/{trace_id}`, `/api/logs`.

## Quick start

```sh
# 1. Postgres
docker compose up -d

# 2. Server (OTLP in on :4318, API out on :4318)
cd server
DATABASE_URL=postgres://watcher:watcher@localhost:5432/watcher cargo run

# 3. UI
cd ui
npm install
npm run dev            # http://localhost:5173  (set VITE_API_BASE if server isn't on :4318)
```

### Send it some test data

Point any OpenTelemetry SDK or the OTel Collector at `http://localhost:4318`, e.g.:

```sh
OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4318 \
OTEL_TRACES_EXPORTER=otlp OTEL_LOGS_EXPORTER=otlp \
  your-app
```

## Status

v0: OTLP/HTTP ingest for traces + logs, Postgres storage, query API, and a UI with a
trace list, trace waterfall, and log search. Not yet: metrics, OTLP/gRPC, auth,
retention/downsampling, service map. See the issues / TODOs.
