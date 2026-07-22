use std::io::IsTerminal;
use std::net::SocketAddr;

use opentelemetry::trace::TracerProvider as _;
use std::sync::Arc;
use tracing_subscriber::filter::dynamic_filter_fn;
use tracing_subscriber::prelude::*;
use tracing_subscriber::EnvFilter;
use watcher_server::{
    access_jwt, alerts, app_with_access, db, grpc, mcp, mcp_auth, retention, selflog, selfmon,
    selftrace,
};

/// Self-instrumentation: capture watcher's own traces **in-process**, tagged
/// `service.name=watcher`, so they land in its own `spans` table and it shows up in
/// its own UI. Like self-metrics (ADR 0014) and self-logs (ADR 0016), traces go
/// straight to the ingest path ([`selftrace`]) — no network hop, no OTLP self-POST,
/// no batch-to-self that can wedge in a shut-down state (JEF-462). Opt out with
/// `WATCHER_SELF_TELEMETRY=0`.
///
/// Returns the provider (kept alive for the process lifetime; dropping it shuts the
/// batch processor down) and the receiver its drain task consumes once the pool is up.
fn init_self_traces() -> Option<(
    opentelemetry_sdk::trace::SdkTracerProvider,
    selftrace::SpanReceiver,
)> {
    if !selftrace::enabled() {
        return None;
    }
    // W3C trace-context propagation, so incoming `traceparent` headers are
    // honored (watcher's spans continue the caller's trace) and our own
    // outbound calls can inject it.
    opentelemetry::global::set_text_map_propagator(
        opentelemetry_sdk::propagation::TraceContextPropagator::new(),
    );
    let (exporter, rx) = selftrace::channel_exporter();
    Some((selftrace::build_provider(exporter), rx))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Capture boot time up front so retention-stall detection measures uptime
    // from here, not from the first /healthz hit.
    selfmon::mark_started();
    let self_traces = init_self_traces();
    let otel_layer = self_traces.as_ref().map(|(provider, _)| {
        // The per-layer filter is the trace analogue of selflog's `on_event` guard:
        // while a self-signal store runs (selflog::store_guarded sets the shared
        // `STORING` task-local), skip capture so a span the store path emits is never
        // exported and re-stored. `dynamic_filter_fn` (not `filter_fn`) is required —
        // it reads the task-local per span rather than caching the callsite decision.
        tracing_opentelemetry::layer()
            .with_tracer(provider.tracer("watcher-server"))
            .with_filter(dynamic_filter_fn(|_meta, _cx| !selflog::suppressed()))
    });
    // Self-log capture (JEF-452): a layer that mirrors watcher's own events into its
    // own `logs` table. Built here, before the pool exists, so startup events buffer
    // in its channel; `main` spawns the drain task once the DB is up. The receiver
    // rides alongside the (Option) layer so both share the enabled() decision.
    let (selflog_layer, selflog_rx) = if selflog::enabled() {
        let (layer, rx) = selflog::channel_layer();
        (Some(layer), Some(rx))
    } else {
        (None, None)
    };
    tracing_subscriber::registry()
        .with(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info,watcher_server=debug,sqlx=warn")),
        )
        // Only colorize when stdout is a real terminal. Under Kubernetes (and any
        // piped/redirected stdout) the fmt layer's default ANSI-on would otherwise
        // litter the pod logs with raw `\x1b[..m` escape sequences. `NO_COLOR`
        // (https://no-color.org) force-disables it regardless.
        .with(
            tracing_subscriber::fmt::layer().with_ansi(
                std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none(),
            ),
        )
        .with(otel_layer)
        .with(selflog_layer)
        .init();

    // Keep the tracer provider alive for the whole process (dropping it shuts the
    // batch processor down — the very failure mode JEF-462 fixes), and take the
    // receiver so the drain task can be spawned once the pool is up.
    let (_self_trace_provider, selftrace_rx) = self_traces.unzip();

    let database_url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://watcher:watcher@localhost:5432/watcher".to_string());
    let http_bind = std::env::var("BIND_ADDR").unwrap_or_else(|_| "0.0.0.0:4318".to_string());
    let grpc_bind: SocketAddr = std::env::var("GRPC_BIND_ADDR")
        .unwrap_or_else(|_| "0.0.0.0:4317".to_string())
        .parse()?;
    let env_i32 = |k: &str, default: i32| -> i32 {
        std::env::var(k)
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(default)
    };
    let retention_days = env_i32("WATCHER_RETENTION_DAYS", 7);
    // Raw metric points are aggregated into per-series rollups on ingest, so raw
    // is kept only as a short full-resolution window for inspection.
    let metrics_raw_hours = env_i32("WATCHER_METRICS_RAW_HOURS", 6);
    // How often to evaluate alert rules, and where to POST when one fires.
    let alert_interval_secs = env_i32("WATCHER_ALERT_INTERVAL_SECS", 30).max(5) as u64;
    let alert_webhook = std::env::var("WATCHER_ALERT_WEBHOOK")
        .ok()
        .filter(|s| !s.is_empty());
    // Optional SMTP delivery of alert transitions, enabled by setting
    // WATCHER_ALERT_SMTP_HOST. The other SMTP_* vars fill in the rest.
    let alert_email = std::env::var("WATCHER_ALERT_SMTP_HOST")
        .ok()
        .filter(|s| !s.is_empty())
        .map(|relay| alerts::EmailConfig {
            relay,
            port: env_i32("WATCHER_ALERT_SMTP_PORT", 587) as u16,
            username: std::env::var("WATCHER_ALERT_SMTP_USERNAME").unwrap_or_default(),
            password: std::env::var("WATCHER_ALERT_SMTP_PASSWORD").unwrap_or_default(),
            from: std::env::var("WATCHER_ALERT_SMTP_FROM").unwrap_or_default(),
            to: std::env::var("WATCHER_ALERT_SMTP_TO").unwrap_or_default(),
        });

    let pool = db::connect(&database_url).await?;
    db::migrate(&pool).await?;

    // Alert rules are declarative: WATCHER_ALERTS_CONFIG points at a JSON file
    // (rendered from the chart's values) that is the source of truth. Reconcile
    // it into alert_rules on startup; a load/parse error is logged but left
    // non-fatal so a bad edit can't take down ingest (the prior rules stand).
    if let Some(path) = std::env::var("WATCHER_ALERTS_CONFIG")
        .ok()
        .filter(|s| !s.is_empty())
    {
        match alerts::load_rules(&path) {
            Ok(rules) => {
                let n = rules.len();
                match alerts::reconcile(&pool, &rules).await {
                    Ok(()) => tracing::info!("reconciled {n} alert rule(s) from {path}"),
                    Err(e) => tracing::error!("alert rule reconcile failed: {e:#}"),
                }
            }
            Err(e) => tracing::error!("alert config load failed: {e:#}"),
        }
    }

    // No downsample sweep: per-series rollups are maintained incrementally on
    // ingest (see otlp::flush_numbers / flush_histograms).
    tokio::spawn(retention::run(
        pool.clone(),
        retention_days,
        metrics_raw_hours,
    ));
    tokio::spawn(alerts::run(
        pool.clone(),
        alert_webhook,
        alert_email,
        alert_interval_secs,
    ));
    // Self-monitoring: emit watcher's own ops gauges/counters over the same OTLP
    // metrics path it ingests, so its health rides its own UI and is alertable.
    if selfmon::enabled() {
        tokio::spawn(selfmon::run(pool.clone()));
    }
    // Self-logs: drain the buffered self-log records (captured by the tracing layer
    // installed above) into the `logs` table via the same in-process ingest path.
    if let Some(rx) = selflog_rx {
        tokio::spawn(selflog::drain(pool.clone(), rx));
    }
    // Self-traces (JEF-462): drain the buffered self-spans (exported by the in-process
    // SpanExporter installed above) into the `spans` table via the same ingest path.
    // Spawned on the main runtime so its sqlx I/O runs on the pool's own reactors.
    if let Some(rx) = selftrace_rx {
        tokio::spawn(selftrace::drain(pool.clone(), rx));
    }

    // Origin-side Cloudflare Access JWT verification (JEF-473): when
    // WATCHER_ACCESS_TEAM_DOMAIN + WATCHER_ACCESS_AUD are set, the UI shell + /api
    // additionally re-verify the edge-issued Access token as defense-in-depth
    // (ADR 0013). Unset → not wired in, so local dev / non-Access deploys are
    // unchanged. /v1 ingest and /healthz are never gated.
    let access = access_jwt::Verifier::from_env().map(Arc::new);
    if access.is_some() {
        tracing::info!("Cloudflare Access origin JWT verification enabled for UI + /api");
    } else {
        tracing::info!(
            "Cloudflare Access origin verification disabled (WATCHER_ACCESS_* unset); \
             edge auth only"
        );
    }

    // Read-only MCP server (JEF-471): mounted at /mcp by `app_with_access` only when
    // WATCHER_MCP_ENABLED is set (default OFF). Its Bearer auth (JEF-472) requires
    // WATCHER_ACCESS_TEAM_DOMAIN + WATCHER_MCP_ACCESS_AUD; with those unset the
    // endpoint fails closed (is NOT served) rather than exposing read access.
    if mcp::enabled() {
        if mcp_auth::McpAuth::from_env().is_some() {
            tracing::info!("MCP server (read-only) enabled at /mcp with Access Bearer auth");
        } else {
            tracing::error!(
                "WATCHER_MCP_ENABLED is set but MCP auth is unconfigured \
                 (need WATCHER_ACCESS_TEAM_DOMAIN + WATCHER_MCP_ACCESS_AUD); \
                 /mcp will NOT be served (fail closed)"
            );
        }
    } else {
        tracing::debug!("MCP server disabled (set WATCHER_MCP_ENABLED=1 to enable /mcp)");
    }

    let http = {
        let pool = pool.clone();
        let bind = http_bind.clone();
        async move {
            let listener = tokio::net::TcpListener::bind(&bind).await?;
            tracing::info!("HTTP/OTLP + API on http://{bind}");
            axum::serve(listener, app_with_access(pool, access)).await?;
            Ok::<(), anyhow::Error>(())
        }
    };
    let grpc = {
        let pool = pool.clone();
        async move {
            tracing::info!("gRPC/OTLP on {grpc_bind}");
            grpc::serve(pool, grpc_bind).await?;
            Ok::<(), anyhow::Error>(())
        }
    };

    tokio::try_join!(http, grpc)?;
    Ok(())
}
