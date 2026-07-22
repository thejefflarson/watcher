# Architecture Decision Records

Short records of the consequential decisions behind watcher — the *why*, not just
the *what*. New decisions get a new numbered file; superseded ones stay (marked
`Superseded by NNNN`) so the history is legible.

Format: [Michael Nygard's ADR style](https://cognitect.com/blog/2011/11/15/documenting-architecture-decisions).
Copy [`0000-template.md`](0000-template.md) to start one.

| # | Decision | Status |
|---|----------|--------|
| [0001](0001-postgres-only-no-clickhouse.md) | Postgres is the only datastore (no ClickHouse) | Accepted |
| [0002](0002-rust-axum-sqlx-runtime-queries.md) | Rust + axum + sqlx with runtime-checked queries | Accepted |
| [0003](0003-denormalized-jsonb-rows.md) | One denormalized row per span/log/metric point, attributes as JSONB | Accepted |
| [0004](0004-otlp-http-and-grpc.md) | Accept OTLP over both HTTP and gRPC, sharing storage code | Accepted |
| [0005](0005-zalando-dedicated-postgres.md) | Dedicated Postgres via the Zalando operator | Accepted |
| [0006](0006-single-origin-ui.md) | Single-origin deployment, path-split by Traefik | Superseded by 0010 |
| [0007](0007-retention-by-deletion.md) | Retention by time-based deletion (defer downsampling) | Accepted |
| [0008](0008-optional-bearer-auth.md) | Optional bearer-token auth, open by default | Superseded by 0013 |
| [0009](0009-minimal-tufte-ui.md) | Minimal, high-data-ink UI | Accepted |
| [0010](0010-ui-embedded-in-server-binary.md) | Embed the UI in the server binary (drop nginx) | Accepted |
| [0011](0011-metric-rollups.md) | Downsample metrics into rollup buckets | Accepted |
| [0012](0012-alerting.md) | Threshold alerting with stored events and optional webhook | Accepted |
| [0013](0013-auth-at-the-edge.md) | Authenticate at the edge (Cloudflare Access), not in the app | Accepted |
| [0014](0014-self-monitoring-in-process-metrics.md) | Self-monitoring: emit ops metrics in-process, deep `/healthz` gates readiness | Accepted |
| [0015](0015-sustained-condition-alerts.md) | Sustained-condition alerts (`for: 5m`) | Accepted |
| [0016](0016-self-log-instrumentation.md) | Self-instrument watcher's own logs in-process | Accepted |
| [0017](0017-self-trace-instrumentation-in-process.md) | Self-instrument watcher's own traces in-process | Accepted |
