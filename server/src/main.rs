use std::net::SocketAddr;

use opentelemetry::trace::TracerProvider as _;
use opentelemetry_otlp::WithExportConfig;
use tracing_subscriber::prelude::*;
use tracing_subscriber::EnvFilter;
use watcher_server::{alerts, app, db, grpc, retention};

/// Self-instrumentation: export watcher's own traces over OTLP, tagged
/// `service.name=watcher` by default, so it shows up in its own UI. Exports to
/// itself (`http://localhost:4318`) unless `OTEL_EXPORTER_OTLP_ENDPOINT` says
/// otherwise; opt out with `WATCHER_SELF_TELEMETRY=0`. Only `/api` requests are
/// spanned (not `/v1`), so exporting to self can't loop.
fn init_telemetry() -> Option<opentelemetry_sdk::trace::TracerProvider> {
    let off = std::env::var("WATCHER_SELF_TELEMETRY")
        .map(|v| matches!(v.as_str(), "0" | "false" | "off"))
        .unwrap_or(false);
    if off {
        return None;
    }
    // W3C trace-context propagation, so incoming `traceparent` headers are
    // honored (watcher's spans continue the caller's trace) and our own
    // outbound calls can inject it.
    opentelemetry::global::set_text_map_propagator(
        opentelemetry_sdk::propagation::TraceContextPropagator::new(),
    );

    let endpoint = std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT")
        .unwrap_or_else(|_| "http://localhost:4318".to_string());
    let service = std::env::var("OTEL_SERVICE_NAME").unwrap_or_else(|_| "watcher".to_string());

    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_http()
        .with_endpoint(format!("{}/v1/traces", endpoint.trim_end_matches('/')))
        .build()
        .ok()?;
    let provider = opentelemetry_sdk::trace::TracerProvider::builder()
        .with_batch_exporter(exporter, opentelemetry_sdk::runtime::Tokio)
        .with_resource(opentelemetry_sdk::Resource::new(vec![
            opentelemetry::KeyValue::new("service.name", service),
        ]))
        .build();
    Some(provider)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let telemetry = init_telemetry();
    let otel_layer = telemetry
        .as_ref()
        .map(|p| tracing_opentelemetry::layer().with_tracer(p.tracer("watcher-server")));
    tracing_subscriber::registry()
        .with(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info,watcher_server=debug,sqlx=warn")),
        )
        .with(tracing_subscriber::fmt::layer())
        .with(otel_layer)
        .init();

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

    let http = {
        let pool = pool.clone();
        let bind = http_bind.clone();
        async move {
            let listener = tokio::net::TcpListener::bind(&bind).await?;
            tracing::info!("HTTP/OTLP + API on http://{bind}");
            axum::serve(listener, app(pool)).await?;
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
