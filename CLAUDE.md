# watcher — Claude context

## What this is

A small, **Postgres-native** OpenTelemetry **traces + logs** backend with a built-in
UI — a SigNoz-style experience without ClickHouse, light enough for a Raspberry Pi.

- **`server/`** — Rust (axum + sqlx). OTLP/HTTP ingest on `:4318` (`/v1/traces`,
  `/v1/logs`, `/v1/metrics`), query API (`/api/...`), background retention + metric
  rollups + alert evaluation, migrations embedded via `sqlx::migrate!` and run on
  startup. The built UI is **embedded into the binary** (`rust-embed`) and served as
  the axum fallback — no separate UI process.
- **`ui/`** — Vite + React + TypeScript. Traces, logs, metrics (table + time-series
  charts), service map, alerts. Built to `ui/dist`, which the server embeds.
- **`chart/`** — Helm chart: one server deployment, a Zalando `postgresql` CR, and a
  Traefik IngressRoute (whole host → server).

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
- The UI calls the API **same-origin** in production (built with `VITE_API_BASE=""`
  and served by the server itself) and `http://localhost:4318` in dev. Because the
  server serves both `/api` and the SPA fallback, same-origin works with or without
  an ingress — don't reintroduce a separate UI process or a path-split. `cargo build`
  needs `ui/dist` to exist; `build.rs` creates an empty placeholder if it's missing.

## CI / images

`.github/workflows/ci.yml`: `cargo fmt/build/test`, UI build (type-check gate),
`helm lint`, and on `main` a multi-arch (amd64 + arm64) image push to GHCR. One image
now — the server build (context = repo root, `server/Dockerfile`) builds the UI and
embeds it:
- `ghcr.io/thejefflarson/watcher-server`

## Deploy

The chart provisions a dedicated Postgres via the Zalando **postgres-operator** and
exposes the UI/OTLP endpoint through **Traefik** — both must exist in the target
cluster. ARM images are required for Raspberry Pi nodes (CI builds them).

## Implemented

Traces, logs, metrics (latest-value table **and** time-series charts), OTLP HTTP +
gRPC, service map, retention, metric downsampling/rollups, threshold alerting
(rules + events + optional webhook). No app-layer auth — the public read surface is
gated by Cloudflare Access at the edge and ingest stays in-cluster (ADR 0013).

## Not yet (good first issues)

Sustained-condition alerts (`for: 5m` rather than a single breach), per-service /
per-signal retention policies, exemplar/trace links from metrics, and webhook
delivery retries.
