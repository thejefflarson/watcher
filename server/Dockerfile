# Single image: builds the UI, then a Rust binary with the UI embedded
# (rust-embed). No nginx, no static-file sidecar — the server serves the API,
# OTLP ingest, and the SPA from one port. Build context is the repo root.
#
#   docker build -f server/Dockerfile -t watcher-server .

# Stage 1 — build the static UI. Same-origin (VITE_API_BASE=""): the server
# serves the API and the UI together, so the UI calls /api, /v1 relatively.
FROM node:22-alpine AS ui
WORKDIR /ui
COPY ui/package.json ui/package-lock.json ./
RUN npm ci
COPY ui/ ./
RUN VITE_API_BASE="" npm run build

# Stage 2 — build the server with cargo-chef. The slow dependency compile is
# split into its own layer (`cook`) keyed on the dependency recipe, so it
# cache-hits on GHA's layer cache (cache-from/to type=gha in CI) until Cargo.lock
# changes. Unlike the per-node BuildKit cache mounts, the GHA layer cache is
# PORTABLE across nodes, so a cold runner reuses the dep compile instead of
# redoing it. Migrations are embedded via sqlx::migrate! and the UI via
# rust-embed, so the runtime image needs only the binary + CA certs.
FROM rust:1-bookworm AS chef
RUN cargo install cargo-chef --locked
WORKDIR /app

FROM chef AS planner
COPY server/Cargo.toml server/Cargo.lock ./
COPY server/build.rs ./build.rs
COPY server/src ./src
# Distill the dependency graph into recipe.json — changes only when deps change,
# so the cook layer below stays cached across ordinary source edits.
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS build
COPY --from=planner /app/recipe.json recipe.json
# Compile ONLY dependencies. The output (populated target dir) is baked into this
# layer — deliberately NOT a target cache mount — so GHA's layer cache carries it
# across nodes. registry/git stay cache mounts: a pure LAN-local speedup for the
# crate download with no effect on the portable layer.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    cargo chef cook --release --recipe-path recipe.json
# Now the source. A source-only change invalidates from here on, but the deps are
# already compiled in target (from the cook layer), so only our crate recompiles.
COPY server/Cargo.toml server/Cargo.lock ./
COPY server/build.rs ./build.rs
COPY server/migrations ./migrations
COPY server/src ./src
COPY --from=ui /ui/dist /ui/dist
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    cargo build --release --locked

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /app/target/release/watcher-server /usr/local/bin/watcher-server
ENV BIND_ADDR=0.0.0.0:4318
EXPOSE 4318
USER 65534:65534
ENTRYPOINT ["/usr/local/bin/watcher-server"]
