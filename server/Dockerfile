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

# Stage 2 — build the server. Dependency-recompile dedup is sccache against the
# shared in-cluster Redis (sccache-redis.dev), not cargo-chef: sccache keys each
# rustc invocation and pulls hits from Redis, so a cold node reuses compiles built
# on any other node — portable like GHA's layer cache, but content-addressed.
# cargo-chef was REMOVED because it can't coexist with sccache: sccache hashes
# every `--extern` input (cargo always passes `.rmeta` metadata externs), and
# chef's `cook` leaves the shared target dir in a state where those extern `.rmeta`
# files don't survive into the final `cargo build`, so sccache fatals ("Failed to
# open file for hashing: …/lib*.rmeta: No such file or directory") and aborts
# (JEF-389, confirmed real and backend-independent). Collapsed to one build stage:
# COPY source, one `cargo build`. Migrations are embedded via sqlx::migrate! and
# the UI via rust-embed, so the runtime image needs only the binary + CA certs.
FROM rust:1-bookworm AS build
WORKDIR /app
# sccache v0.16.0 (arch-aware; -gnu because the rust base is bookworm/glibc, not
# musl). RUSTC_WRAPPER routes every rustc through sccache; CARGO_INCREMENTAL=0 is
# mandatory (incremental artifacts defeat sccache's cache keys).
RUN set -eux; ver=0.16.0; \
    case "$(uname -m)" in x86_64) a=x86_64 ;; aarch64) a=aarch64 ;; *) echo "unsupported arch $(uname -m)" >&2; exit 1 ;; esac; \
    wget -qO- "https://github.com/mozilla/sccache/releases/download/v${ver}/sccache-v${ver}-${a}-unknown-linux-gnu.tar.gz" \
      | tar -xz -C /usr/local/bin --strip-components=1 "sccache-v${ver}-${a}-unknown-linux-gnu/sccache"
ENV RUSTC_WRAPPER=sccache CARGO_INCREMENTAL=0 \
    SCCACHE_REDIS=redis://sccache-redis.dev.svc.cluster.local:6379
COPY server/Cargo.toml server/Cargo.lock ./
COPY server/build.rs ./build.rs
COPY server/migrations ./migrations
COPY server/src ./src
COPY --from=ui /ui/dist /ui/dist
# sccache is a HARD GATE — no local fallback; if it can't reach Redis the build
# fails. SCCACHE_SERVER_PORT is randomised per build because BuildKit sandboxes
# share a network namespace, so concurrent builds would collide on sccache's fixed
# default port 4226 ("Address in use").
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    set -e; \
    export SCCACHE_SERVER_PORT=$(awk 'BEGIN{srand(); print int(20000+rand()*40000)}'); \
    timeout 10 sccache --start-server; \
    cargo build --release --locked; \
    sccache --show-stats

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /app/target/release/watcher-server /usr/local/bin/watcher-server
ENV BIND_ADDR=0.0.0.0:4318
EXPOSE 4318
USER 65534:65534
ENTRYPOINT ["/usr/local/bin/watcher-server"]
