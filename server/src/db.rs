use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions};
use std::str::FromStr;
use std::time::Duration;

pub async fn connect(url: &str) -> anyhow::Result<PgPool> {
    // statement_timeout bounds any single statement so a pathological ingest batch
    // or query can't pin one of the (few) pool connections indefinitely — a handful
    // of stuck statements would otherwise exhaust the pool and stall everything.
    // 60s is far above a normal sub-second read or the size-capped ingest batch, yet
    // still leaves ample room for the hourly retention sweep's larger DELETEs.
    let opts = PgConnectOptions::from_str(url)?.options([("statement_timeout", "60s")]);
    let pool = PgPoolOptions::new()
        .max_connections(10)
        // Defense-in-depth for a Patroni/postgres-operator failover (JEF-496). When
        // the leader is demoted, the pool keeps live connections to what is now a
        // read-only replica; writes on them fail with SQLSTATE 25006 until the
        // connections recycle and the `-master` DNS re-resolves to the new leader.
        // The ingest write path retries 25006 on a fresh connection (otlp.rs), but
        // these two settings make the whole pool rotate off the demoted node
        // promptly even for idle connections and even absent a write:
        //
        // * max_lifetime caps how long any connection lives regardless of health, so
        //   a still-open but demoted (read-only) connection is closed and re-opened —
        //   re-resolving `-master` to the new leader — within this window. 5 minutes
        //   is long enough not to churn connections under steady ingest (each survives
        //   thousands of batches) yet bounds post-failover staleness to minutes.
        // * test_before_acquire (sqlx's default, set explicitly here so the intent is
        //   visible and survives a default change) pings each connection on checkout,
        //   so one the old leader *closed* on demotion (57P01) is discarded and
        //   replaced before a writer ever sees it. A ping alone can't catch a still-open
        //   read-only backend — that's what the 25006 write retry is for.
        .max_lifetime(Duration::from_secs(300))
        .test_before_acquire(true)
        .connect_with(opts)
        .await?;
    Ok(pool)
}

pub async fn migrate(pool: &PgPool) -> anyhow::Result<()> {
    sqlx::migrate!("./migrations").run(pool).await?;
    Ok(())
}
