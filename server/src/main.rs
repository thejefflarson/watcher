use std::net::SocketAddr;

use tracing_subscriber::EnvFilter;
use watcher_server::{alerts, app, db, grpc, retention, rollup};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info,watcher_server=debug,sqlx=warn")),
        )
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
    // Raw metric points are pruned sooner than everything else; rollups keep the
    // history. 0 keeps raw points for the full retention window.
    let metrics_raw_days = env_i32("WATCHER_METRICS_RAW_DAYS", 2);
    // Width of a downsample bucket, in seconds (0 disables rollups).
    let rollup_bucket_secs = env_i32("WATCHER_ROLLUP_BUCKET_SECS", 300) as i64;
    // How often to evaluate alert rules, and where to POST when one fires.
    let alert_interval_secs = env_i32("WATCHER_ALERT_INTERVAL_SECS", 30).max(5) as u64;
    let alert_webhook = std::env::var("WATCHER_ALERT_WEBHOOK")
        .ok()
        .filter(|s| !s.is_empty());

    let pool = db::connect(&database_url).await?;
    db::migrate(&pool).await?;

    tokio::spawn(retention::run(
        pool.clone(),
        retention_days,
        metrics_raw_days,
    ));
    tokio::spawn(rollup::run(pool.clone(), rollup_bucket_secs));
    tokio::spawn(alerts::run(
        pool.clone(),
        alert_webhook,
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
