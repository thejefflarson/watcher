use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions};
use std::str::FromStr;

pub async fn connect(url: &str) -> anyhow::Result<PgPool> {
    // statement_timeout bounds any single statement so a pathological ingest batch
    // or query can't pin one of the (few) pool connections indefinitely — a handful
    // of stuck statements would otherwise exhaust the pool and stall everything.
    // 60s is far above a normal sub-second read or the size-capped ingest batch, yet
    // still leaves ample room for the hourly retention sweep's larger DELETEs.
    let opts = PgConnectOptions::from_str(url)?.options([("statement_timeout", "60s")]);
    let pool = PgPoolOptions::new()
        .max_connections(10)
        .connect_with(opts)
        .await?;
    Ok(pool)
}

pub async fn migrate(pool: &PgPool) -> anyhow::Result<()> {
    sqlx::migrate!("./migrations").run(pool).await?;
    Ok(())
}
