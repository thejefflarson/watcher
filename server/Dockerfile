# Multi-stage build for the Rust ingest/query server.
# Migrations are embedded into the binary at compile time (sqlx::migrate!),
# so the runtime image needs only the binary + CA certs.
FROM rust:1-bookworm AS build
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY migrations ./migrations
COPY src ./src
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
