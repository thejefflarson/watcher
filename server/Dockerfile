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

# Stage 2 — build the server. Migrations are embedded via sqlx::migrate! and the
# UI via rust-embed (folder "../ui/dist" relative to the crate), so the runtime
# image needs only the binary + CA certs.
FROM rust:1-bookworm AS build
WORKDIR /app
COPY server/Cargo.toml server/Cargo.lock ./
COPY server/build.rs ./build.rs
COPY server/migrations ./migrations
COPY server/src ./src
COPY --from=ui /ui/dist /ui/dist
RUN cargo build --release --locked

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /app/target/release/watcher-server /usr/local/bin/watcher-server
ENV BIND_ADDR=0.0.0.0:4318
EXPOSE 4318
USER 65534:65534
ENTRYPOINT ["/usr/local/bin/watcher-server"]
