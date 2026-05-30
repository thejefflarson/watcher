# 0002. Rust + axum + sqlx with runtime-checked queries

- Status: Accepted
- Date: 2026-05-30

## Context

The ingest path must decode OTLP protobuf cheaply and write to Postgres with low
overhead on a Pi. We want a single small static binary. sqlx offers compile-time
query checking via the `query!` macros, but that requires a live database (or a
checked-in `.sqlx` offline cache) at **build time** — friction for CI and for a
multi-stage Docker build.

## Decision

We will use **Rust** with **axum** (HTTP) and **sqlx** against Postgres, using the
**runtime** query API (`sqlx::query` / `query_as`), not the compile-time macros.
Migrations are embedded with `sqlx::migrate!` and run on startup.

## Consequences

- `cargo check` / `cargo build` need no database; the Docker build is a plain
  multi-stage compile.
- We lose compile-time SQL verification. We compensate with **integration tests**
  (`tests/smoke.rs`) that run real queries against a Postgres service in CI — which
  has already caught real bugs (e.g. Postgres 14+ `extract()` returning `numeric`).
- Schema changes are additive numbered files in `server/migrations/`; never edit a
  shipped migration.
