use sqlx::postgres::{PgConnectOptions, PgConnection, PgPool, PgPoolOptions};
use sqlx::Connection;
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
        // Defense-in-depth for a Patroni/postgres-operator failover. When
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

/// Run pending migrations on a single dedicated connection — deliberately
/// NOT the query pool from [`connect`] above, so a migration never inherits
/// that pool's 60s `statement_timeout` or waits behind ingest traffic for a
/// lock. (Migration 0017's `CREATE INDEX` ran on the shared pool,
/// inherited the 60s bound, and was killed mid-run — crashlooping the pod;
/// see ADR 0021.)
///
/// * `lock_timeout=3s`: a migration that can't acquire the lock it needs
///   (e.g. blocked behind a long-running transaction on the target table)
///   aborts fast and fails startup loudly, instead of queuing ahead of
///   ingest and head-of-line-blocking that table for as long as the
///   competing lock is held.
/// * `statement_timeout=0` (unbounded), decoupled from the query pool's 60s:
///   a legitimate transactional migration on a large table shouldn't be
///   capped at the app's query-latency bound. The policy (ADR 0021) is that
///   heavy/locking DDL — anything that needs `CREATE INDEX CONCURRENTLY` or
///   similarly can't run inside a migration's transaction — belongs in the
///   separate online-DDL lane, not here, so what runs via `sqlx::migrate!`
///   is expected to stay short regardless of the unbounded timeout.
///
/// Migrations apply serially at startup, so one connection — not a pool —
/// is all this needs.
pub async fn migrate(url: &str) -> anyhow::Result<()> {
    let mut conn = PgConnection::connect_with(&migrate_connect_options(url)?).await?;
    sqlx::migrate!("./migrations").run(&mut conn).await?;
    Ok(())
}

/// The connect options [`migrate`] runs on — split out so a test can open a
/// connection with these exact settings without going through a real migration.
fn migrate_connect_options(url: &str) -> anyhow::Result<PgConnectOptions> {
    Ok(PgConnectOptions::from_str(url)?
        .options([("lock_timeout", "3s"), ("statement_timeout", "0")]))
}

/// Connect options for the online-DDL lane (ADR 0021): a dedicated
/// connection, not the query pool, so a `CREATE`/`DROP INDEX CONCURRENTLY` build
/// that runs for minutes on a big table is never cancelled by the pool's 60s
/// `statement_timeout` (exactly the failure a plain, transactional `CREATE INDEX`
/// hit on migration 0017). `lock_timeout=3s` matches [`migrate_connect_options`]:
/// a build that can't get the lock it needs (e.g. blocked behind a long-running
/// transaction) aborts fast rather than queuing ahead of ingest.
/// `maintenance_work_mem` is capped explicitly rather than inherited from the
/// server-wide default, which stays conservative for a Raspberry Pi's limited RAM;
/// an index build can afford more without raising that default for every backend.
pub fn online_ddl_connect_options(url: &str) -> anyhow::Result<PgConnectOptions> {
    Ok(PgConnectOptions::from_str(url)?.options([
        ("lock_timeout", "3s"),
        ("statement_timeout", "0"),
        ("maintenance_work_mem", "64MB"),
    ]))
}

#[cfg(test)]
mod tests {
    use super::{migrate_connect_options, PgConnection};
    use sqlx::Connection;
    use std::time::{Duration, Instant};

    /// A migration that can't get the lock it needs must abort within
    /// `lock_timeout`, not queue behind the holder and head-of-line-block the
    /// table. Proven at the connection level (no real migration
    /// file needed): a competing transaction takes an ACCESS EXCLUSIVE lock on a
    /// throwaway table, and a trivial DDL statement on a connection configured
    /// exactly like `migrate`'s must fail fast with a lock_timeout error rather
    /// than hang for the test's duration.
    #[tokio::test]
    async fn migrate_connection_aborts_fast_on_a_held_lock() {
        let Ok(url) = std::env::var("DATABASE_URL") else {
            eprintln!("skipping: DATABASE_URL not set");
            return;
        };

        // Holds a conflicting lock on a throwaway table until we roll it back below.
        let mut blocker = PgConnection::connect(&url).await.expect("connect blocker");
        sqlx::query("CREATE TABLE IF NOT EXISTS jef_590_lock_test (id int)")
            .execute(&mut blocker)
            .await
            .expect("create throwaway table");
        sqlx::query("BEGIN")
            .execute(&mut blocker)
            .await
            .expect("begin");
        sqlx::query("LOCK TABLE jef_590_lock_test IN ACCESS EXCLUSIVE MODE")
            .execute(&mut blocker)
            .await
            .expect("lock");

        // A connection with exactly migrate()'s settings, attempting a trivial DDL
        // against the table the blocker holds locked.
        let opts = migrate_connect_options(&url).expect("connect options");
        let mut migrate_conn = PgConnection::connect_with(&opts)
            .await
            .expect("connect migrate-style");

        let start = Instant::now();
        let result = sqlx::query("ALTER TABLE jef_590_lock_test ADD COLUMN probe int")
            .execute(&mut migrate_conn)
            .await;
        let elapsed = start.elapsed();

        // Release the lock and clean up regardless of what the assertions below find.
        let _ = sqlx::query("ROLLBACK").execute(&mut blocker).await;
        let _ = sqlx::query("DROP TABLE IF EXISTS jef_590_lock_test")
            .execute(&mut blocker)
            .await;

        let err = result.expect_err("DDL against a held conflicting lock must error, not hang");
        assert!(
            err.to_string().to_lowercase().contains("lock timeout"),
            "expected a lock_timeout error, got: {err}"
        );
        assert!(
            elapsed < Duration::from_secs(10),
            "must abort within lock_timeout (3s), took {elapsed:?}"
        );
    }
}
