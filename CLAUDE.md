# watcher — Claude context

## What this is

A small, **Postgres-native** OpenTelemetry **traces + logs** backend with a built-in
UI — a SigNoz-style experience without ClickHouse, light enough for a Raspberry Pi.

- **`server/`** — Rust (axum + sqlx). OTLP/HTTP ingest on `:4318` (`/v1/traces`,
  `/v1/logs`), query API (`/api/...`), migrations embedded via `sqlx::migrate!` and run
  on startup.
- **`ui/`** — Vite + React + TypeScript. Trace list, trace waterfall, log search.
- **`chart/`** — Helm chart: server + UI deployments, a Zalando `postgresql` CR, and a
  Traefik IngressRoute.

## Key commands

```sh
# Backend
cd server && cargo check         # after every edit
cargo fmt                        # before committing
cargo test --locked

# UI
cd ui && npm run build           # = tsc --noEmit && vite build

# Local dev DB + run
docker compose up -d             # Postgres on :5432
cd server && DATABASE_URL=postgres://watcher:watcher@localhost:5432/watcher cargo run

# Chart
helm lint chart
helm template watcher chart
```

## Conventions

- Rust: run `cargo fmt` + `cargo check` before committing; treat warnings as errors.
- SQL: **runtime** sqlx queries (no compile-time DB needed). Schema lives in
  `server/migrations/`; add a new numbered file rather than editing old ones.
- The server builds `DATABASE_URL` itself; in Kubernetes the password comes from the
  Zalando credential secret and is composed in via `$(PGPASSWORD)` env expansion.
- The UI calls the API **same-origin** in production (built with `VITE_API_BASE=""`,
  path-split by the IngressRoute) and `http://localhost:4318` in dev.

## CI / images

`.github/workflows/ci.yml`: `cargo fmt/build/test`, UI build, `helm lint`, and on `main`
a multi-arch (amd64 + arm64) image push to GHCR:
- `ghcr.io/thejefflarson/watcher-server`
- `ghcr.io/thejefflarson/watcher-ui`

## Deploy

The chart provisions a dedicated Postgres via the Zalando **postgres-operator** and
exposes the UI/OTLP endpoint through **Traefik** — both must exist in the target
cluster. ARM images are required for Raspberry Pi nodes (CI builds them).

## Not yet (good first issues)

Downsampling/rollups for old data, metric time-series charts in the UI (today it's a
latest-value table), and alerting. Traces, logs, metrics, OTLP/gRPC, service map,
retention, and optional bearer-token auth are all implemented.
