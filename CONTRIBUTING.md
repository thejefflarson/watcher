# Contributing

## Layout

```
server/   Rust ingest + query server (axum + sqlx)
ui/       Vite + React + TS UI
docs/     architecture.md + adr/ (decision records)
```

## Develop

```sh
docker compose up -d                                            # Postgres :5432
cd server && DATABASE_URL=postgres://watcher:watcher@localhost:5432/watcher cargo run
cd ui && npm install && npm run dev                             # http://localhost:5173
```

Send it data by pointing any OTLP exporter at `http://localhost:4318`.

## Before you push

- **server**: `cargo fmt` · `cargo check` · `cargo test` (tests need a Postgres on
  `DATABASE_URL`; they skip cleanly without one, but CI runs them for real).
- **ui**: `npm run build` (runs `tsc --noEmit && vite build`).

CI runs all of the above plus a multi-arch image build on `main`. Treat compiler
warnings as errors.

## Conventions

- **SQL**: runtime sqlx queries ([ADR 0002](docs/adr/0002-rust-axum-sqlx-runtime-queries.md)).
  New schema = a new numbered file in `server/migrations/`; never edit a shipped one.
- **Transports stay thin**: put ingest behavior in `otlp::store_*`, not in the HTTP
  or gRPC layer ([ADR 0004](docs/adr/0004-otlp-http-and-grpc.md)).
- **UI**: minimal/Tufte — reserve color for data, no chartjunk
  ([ADR 0009](docs/adr/0009-minimal-tufte-ui.md)).

## Making a decision

Significant choices get an ADR — copy `docs/adr/0000-template.md`, add a row to
`docs/adr/README.md`.

## Good first issues

Downsampling/rollups (Timescale continuous aggregates), metric time-series charts
in the UI, alerting, and setting `service.name` properly on emitters.
