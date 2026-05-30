use std::net::SocketAddr;

use tracing_subscriber::EnvFilter;
use watcher_server::{app, db, grpc, retention, AuthConfig};

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
    let retention_days: i32 = std::env::var("WATCHER_RETENTION_DAYS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(7);

    let pool = db::connect(&database_url).await?;
    db::migrate(&pool).await?;

    let auth = AuthConfig::from_env();
    tokio::spawn(retention::run(pool.clone(), retention_days));

    let http = {
        let pool = pool.clone();
        let auth = auth.clone();
        let bind = http_bind.clone();
        async move {
            let listener = tokio::net::TcpListener::bind(&bind).await?;
            tracing::info!("HTTP/OTLP + API on http://{bind}");
            axum::serve(listener, app(pool, auth)).await?;
            Ok::<(), anyhow::Error>(())
        }
    };
    let grpc = {
        let pool = pool.clone();
        let ingest = auth.ingest.clone();
        async move {
            tracing::info!("gRPC/OTLP on {grpc_bind}");
            grpc::serve(pool, grpc_bind, ingest).await?;
            Ok::<(), anyhow::Error>(())
        }
    };

    tokio::try_join!(http, grpc)?;
    Ok(())
}
