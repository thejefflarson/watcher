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
# (confirmed real and backend-independent). Collapsed to one build stage:
# COPY source, one `cargo build`. Migrations are embedded via sqlx::migrate! and
# the UI via rust-embed, so the runtime image needs only the binary + CA certs.
FROM rust:1-bookworm AS build
WORKDIR /app
# sccache v0.16.0 (arch-aware; -gnu because the rust base is bookworm/glibc, not
# musl). RUSTC_WRAPPER routes every rustc through sccache; CARGO_INCREMENTAL=0 is
# mandatory (incremental artifacts defeat sccache's cache keys).
RUN set -eux; ver=0.16.0; \
    case "$(uname -m)" in x86_64) a=x86_64 ;; aarch64) a=aarch64 ;; *) echo "unsupported arch $(uname -m)" >&2; exit 1 ;; esac; \
    wget -qO- "https://github.com/mozilla/sccache/releases/download/v${ver}/sccache-v${ver}-${a}-unknown-linux-musl.tar.gz" \
      | tar -xz -C /usr/local/bin --strip-components=1 "sccache-v${ver}-${a}-unknown-linux-musl/sccache"
ENV RUSTC_WRAPPER=sccache CARGO_INCREMENTAL=0
COPY scripts/start-sccache-docker.sh ./scripts/
COPY server/Cargo.toml server/Cargo.lock ./
COPY server/build.rs ./build.rs
COPY server/migrations ./migrations
COPY server/src ./src
COPY --from=ui /ui/dist /ui/dist
# sccache backend = the shared Cloudflare R2 bucket (cluster repo: charts/sccache,
# ADR-0020), replacing the in-cluster Redis this used to hardcode. The config
# and the bucket-scoped token arrive as BuildKit build SECRETS — never ENV or a
# build-arg, both of which persist in `docker history` on every image we push to ghcr.
#
# It is also no longer a HARD GATE. Redis was an in-cluster Service reachable with no
# credentials, so "can't reach it" really did mean "this builder is misconfigured" and
# failing was the honest signal. R2 is a remote bucket behind a rotatable token, where a
# transient blip or a missing `sccache-r2` Secret would fail a build for a reason that
# has nothing to do with the code — while degrading costs one cold compile, exactly what
# a cache miss already costs. scripts/start-sccache-docker.sh probes, retries, then
# degrades to a local disk cache. See its header, and its --selftest fixtures.
#
# SCCACHE_SERVER_PORT is randomised per build because BuildKit sandboxes share a
# network namespace, so concurrent builds would collide on sccache's fixed default
# port 4226 ("Address in use").
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=secret,id=AWS_ACCESS_KEY_ID \
    --mount=type=secret,id=AWS_SECRET_ACCESS_KEY \
    --mount=type=secret,id=SCCACHE_BUCKET \
    --mount=type=secret,id=SCCACHE_ENDPOINT \
    --mount=type=secret,id=SCCACHE_REGION \
    --mount=type=secret,id=SCCACHE_S3_KEY_PREFIX \
    --mount=type=secret,id=SCCACHE_S3_USE_SSL \
    set -e; \
    export SCCACHE_SERVER_PORT=$(awk 'BEGIN{srand(); print int(20000+rand()*40000)}'); \
    sh scripts/start-sccache-docker.sh; \
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
