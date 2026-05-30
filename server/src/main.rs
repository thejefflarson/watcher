use tracing_subscriber::EnvFilter;
use watcher_server::{app, db};

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
    let bind = std::env::var("BIND_ADDR").unwrap_or_else(|_| "0.0.0.0:4318".to_string());

    let pool = db::connect(&database_url).await?;
    db::migrate(&pool).await?;

    let listener = tokio::net::TcpListener::bind(&bind).await?;
    tracing::info!("watcher listening on http://{bind}  (OTLP/HTTP + /api)");
    axum::serve(listener, app(pool)).await?;
    Ok(())
}
