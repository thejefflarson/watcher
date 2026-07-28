//! In-app online (non-transactional) DDL lane (ADR 0021, JEF-580).
//!
//! `sqlx::migrate!` (see [`crate::db::migrate`]) wraps every migration in a
//! transaction and holds a session-level advisory lock for the whole run — the
//! wrong place for `CREATE INDEX CONCURRENTLY`, which cannot run inside a
//! transaction at all, and the wrong place for a build that takes minutes on a
//! multi-million-row table (a plain, transactional `CREATE INDEX` there holds a
//! `SHARE` lock that blocks writes for the whole build — migration 0017's
//! `metric_series_rollups_name_bucket_covering_idx`, see its migration file).
//!
//! This module is a declarative list of the indexes the app wants to exist
//! ([`DESIRED_INDEXES`]) plus a runner ([`run`]) that reconciles reality against
//! that list on a dedicated connection, outside any transaction, after the HTTP
//! listener has already bound (wired in `main.rs`) — so an index build never
//! blocks boot or `/healthz`, and a failure here is logged, not fatal (reads
//! just fall back to whatever index already covers the query, slower but not
//! broken).
//!
//! Reconciliation is driven by `pg_index.indisvalid`, not `IF NOT EXISTS`: a
//! pod killed mid-build leaves an index that's present but `INVALID` — `IF NOT
//! EXISTS` would treat that as "done" forever. This runner instead drops and
//! rebuilds an `INVALID` index, so an interrupted build self-heals on the next
//! run.

use crate::db;
use sqlx::postgres::PgConnection;
use sqlx::Connection;

/// A single desired online index: its name, the table it's built on (for
/// logging), and the exact `CREATE INDEX CONCURRENTLY` statement that builds it.
pub struct OnlineIndex {
    pub name: &'static str,
    pub table: &'static str,
    pub create_sql: &'static str,
}

/// The indexes this lane manages. Seeded with the covering index migration 0017
/// already built by hand in production (as a plain `CREATE INDEX` — see that
/// migration's comments): it exists and is valid there already, so the lane's
/// first run against prod is a no-op that proves the mechanism without changing
/// anything. Definition copied verbatim from 0017 — do not narrow it here.
/// (JEF-591 is a follow-up that edits this entry to drop unused `INCLUDE`
/// columns and relies on the lane to rebuild it; out of scope for this ticket.)
pub const DESIRED_INDEXES: &[OnlineIndex] = &[OnlineIndex {
    name: "metric_series_rollups_name_bucket_covering_idx",
    table: "metric_series_rollups",
    create_sql: "CREATE INDEX CONCURRENTLY metric_series_rollups_name_bucket_covering_idx \
                 ON metric_series_rollups (name, bucket) \
                 INCLUDE (service, kind, unit, is_monotonic, count, sum, avg, max, attrs, \
                 bucket_bounds, bucket_counts)",
}];

/// Session-level advisory-lock key that gates a whole lane run, so exactly one
/// replica builds during a rollout and the others skip cleanly. Arbitrary but
/// fixed — the bytes of "JEF_580" read as a big-endian integer — and
/// deliberately NOT the key `sqlx::migrate!` uses (which it derives at runtime
/// from a hash of the database name, not a static constant), so the two can
/// never collide.
const ADVISORY_LOCK_KEY: i64 = 0x004A_4546_5F35_3830;

/// Whether a desired index is missing, present and healthy, or present but left
/// behind `INVALID` by an interrupted `CONCURRENTLY` build.
#[derive(Debug, PartialEq, Eq)]
enum IndexState {
    Absent,
    Valid,
    Invalid,
}

/// Looks up `name` in `pg_index`/`pg_class` rather than trusting `IF NOT
/// EXISTS`, which can't distinguish "doesn't exist" from "exists but invalid" —
/// see the module docs.
async fn index_state(conn: &mut PgConnection, name: &str) -> anyhow::Result<IndexState> {
    let valid: Option<bool> = sqlx::query_scalar(
        "SELECT i.indisvalid FROM pg_index i \
         JOIN pg_class c ON c.oid = i.indexrelid \
         WHERE c.relname = $1",
    )
    .bind(name)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(match valid {
        None => IndexState::Absent,
        Some(true) => IndexState::Valid,
        Some(false) => IndexState::Invalid,
    })
}

/// Reconciles one desired index against its current state on `conn`. Absent →
/// build; valid → no-op; invalid (left by an interrupted build) → drop then
/// rebuild. Runs entirely outside a transaction, as `CONCURRENTLY` requires.
async fn reconcile_index(conn: &mut PgConnection, idx: &OnlineIndex) -> anyhow::Result<()> {
    match index_state(conn, idx.name).await? {
        IndexState::Valid => {
            tracing::debug!(index = idx.name, "online-ddl: already valid, no-op");
            Ok(())
        }
        IndexState::Absent => build_index(conn, idx).await,
        IndexState::Invalid => {
            tracing::warn!(
                index = idx.name,
                "online-ddl: exists but INVALID (interrupted build); dropping to rebuild"
            );
            // AssertSqlSafe: name is interpolated only from our own hardcoded
            // OnlineIndex constants (never user input), same as retention.rs's
            // table-name interpolation.
            let drop_sql = format!("DROP INDEX CONCURRENTLY {}", idx.name);
            sqlx::query(sqlx::AssertSqlSafe(drop_sql))
                .execute(&mut *conn)
                .await?;
            build_index(conn, idx).await
        }
    }
}

/// Issues the `CREATE INDEX CONCURRENTLY` for `idx` and logs its duration.
async fn build_index(conn: &mut PgConnection, idx: &OnlineIndex) -> anyhow::Result<()> {
    let start = std::time::Instant::now();
    tracing::info!(
        index = idx.name,
        table = idx.table,
        "online-ddl: build started"
    );
    sqlx::query(idx.create_sql).execute(conn).await?;
    tracing::info!(index = idx.name, elapsed = ?start.elapsed(), "online-ddl: build finished");
    Ok(())
}

/// Runs the lane: connects, tries the advisory lock, and (only if acquired)
/// reconciles every entry in `desired` in order. Split out from [`run`] so a
/// test can pass a throwaway index list instead of [`DESIRED_INDEXES`].
async fn run_indexes(url: &str, desired: &[OnlineIndex]) -> anyhow::Result<()> {
    let mut conn = PgConnection::connect_with(&db::online_ddl_connect_options(url)?).await?;

    // Try, don't wait: if another replica already holds this key (e.g. mid-rollout
    // with two pods briefly up), skip cleanly rather than queuing behind its build.
    let acquired: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
        .bind(ADVISORY_LOCK_KEY)
        .fetch_one(&mut conn)
        .await?;
    if !acquired {
        tracing::info!("online-ddl: lock held by another replica, skipping this run");
        return Ok(());
    }

    // A failure on one index is logged and does not stop the rest — a bad
    // definition for one entry shouldn't block a healthy build of another, and
    // this whole lane is already fail-soft from the caller's perspective (see
    // `main.rs`).
    for idx in desired {
        if let Err(e) = reconcile_index(&mut conn, idx).await {
            tracing::error!(index = idx.name, "online-ddl: reconcile failed: {e:#}");
        }
    }

    sqlx::query("SELECT pg_advisory_unlock($1)")
        .bind(ADVISORY_LOCK_KEY)
        .execute(&mut conn)
        .await?;
    Ok(())
}

/// Entry point wired into `main.rs`: spawned as a background task after the
/// HTTP listener binds, so boot and readiness never wait on an index build.
pub async fn run(url: &str) -> anyhow::Result<()> {
    run_indexes(url, DESIRED_INDEXES).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    // These tests share the process-wide advisory lock key and a couple of
    // fixed throwaway table/index names, so they must not run concurrently
    // with each other — `#[serial(online_ddl)]` scopes that to just this
    // module's tests, without forcing serialization against unrelated tests
    // elsewhere in the crate.

    async fn conn_or_skip() -> Option<PgConnection> {
        let url = std::env::var("DATABASE_URL").ok()?;
        Some(
            PgConnection::connect_with(&db::online_ddl_connect_options(&url).expect("opts"))
                .await
                .expect("connect"),
        )
    }

    async fn setup_throwaway_table(conn: &mut PgConnection, table: &str) {
        let _ = sqlx::query(sqlx::AssertSqlSafe(format!("DROP TABLE IF EXISTS {table}")))
            .execute(&mut *conn)
            .await;
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "CREATE TABLE {table} (id int)"
        )))
        .execute(&mut *conn)
        .await
        .expect("create throwaway table");
    }

    async fn drop_throwaway_table(conn: &mut PgConnection, table: &str) {
        let _ = sqlx::query(sqlx::AssertSqlSafe(format!(
            "DROP TABLE IF EXISTS {table} CASCADE"
        )))
        .execute(&mut *conn)
        .await;
    }

    /// The `pg_class` oid of the relation named `name`, used to prove a reconcile
    /// left an index untouched (a rebuild would produce a new oid).
    async fn relation_oid(conn: &mut PgConnection, name: &str) -> i64 {
        sqlx::query_scalar("SELECT oid::bigint FROM pg_class WHERE relname = $1")
            .bind(name)
            .fetch_one(conn)
            .await
            .expect("relation oid")
    }

    #[tokio::test]
    #[serial(online_ddl)]
    async fn absent_index_is_built() {
        let Some(mut conn) = conn_or_skip().await else {
            eprintln!("skipping: DATABASE_URL not set");
            return;
        };
        let table = "jef580_absent_test";
        let name = "jef580_absent_test_idx";
        setup_throwaway_table(&mut conn, table).await;

        assert_eq!(
            index_state(&mut conn, name).await.unwrap(),
            IndexState::Absent
        );

        let idx = OnlineIndex {
            name,
            table,
            create_sql:
                "CREATE INDEX CONCURRENTLY jef580_absent_test_idx ON jef580_absent_test (id)",
        };
        reconcile_index(&mut conn, &idx).await.expect("build");

        assert_eq!(
            index_state(&mut conn, name).await.unwrap(),
            IndexState::Valid
        );
        drop_throwaway_table(&mut conn, table).await;
    }

    #[tokio::test]
    #[serial(online_ddl)]
    async fn valid_index_is_a_noop() {
        let Some(mut conn) = conn_or_skip().await else {
            eprintln!("skipping: DATABASE_URL not set");
            return;
        };
        let table = "jef580_valid_test";
        let name = "jef580_valid_test_idx";
        setup_throwaway_table(&mut conn, table).await;

        let idx = OnlineIndex {
            name,
            table,
            create_sql: "CREATE INDEX CONCURRENTLY jef580_valid_test_idx ON jef580_valid_test (id)",
        };
        // Pre-build it, mirroring the seeded covering index already existing in prod.
        reconcile_index(&mut conn, &idx)
            .await
            .expect("initial build");
        let oid_before = relation_oid(&mut conn, name).await;

        reconcile_index(&mut conn, &idx)
            .await
            .expect("reconcile is a no-op");

        assert_eq!(
            index_state(&mut conn, name).await.unwrap(),
            IndexState::Valid
        );
        assert_eq!(
            oid_before,
            relation_oid(&mut conn, name).await,
            "a no-op reconcile must not drop/rebuild an already-valid index"
        );
        drop_throwaway_table(&mut conn, table).await;
    }

    #[tokio::test]
    #[serial(online_ddl)]
    async fn invalid_index_is_dropped_and_rebuilt() {
        let Some(mut conn) = conn_or_skip().await else {
            eprintln!("skipping: DATABASE_URL not set");
            return;
        };
        let table = "jef580_invalid_test";
        let name = "jef580_invalid_test_idx";
        setup_throwaway_table(&mut conn, table).await;
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "INSERT INTO {table} (id) VALUES (1), (1)"
        )))
        .execute(&mut conn)
        .await
        .expect("insert duplicate rows");

        let idx = OnlineIndex {
            name,
            table,
            create_sql:
                "CREATE UNIQUE INDEX CONCURRENTLY jef580_invalid_test_idx ON jef580_invalid_test (id)",
        };
        // A duplicate-key violation fails the concurrent build partway through,
        // leaving the index present but INVALID — exactly what an interrupted
        // build (e.g. a pod killed mid-build) leaves behind.
        let _ = sqlx::query(idx.create_sql).execute(&mut conn).await;
        assert_eq!(
            index_state(&mut conn, name).await.unwrap(),
            IndexState::Invalid
        );

        // Remove the duplicate so the rebuild the runner triggers can succeed.
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "DELETE FROM {table} WHERE ctid NOT IN (SELECT min(ctid) FROM {table} GROUP BY id)"
        )))
        .execute(&mut conn)
        .await
        .expect("dedupe");

        reconcile_index(&mut conn, &idx)
            .await
            .expect("drop + rebuild");

        assert_eq!(
            index_state(&mut conn, name).await.unwrap(),
            IndexState::Valid
        );
        drop_throwaway_table(&mut conn, table).await;
    }

    #[tokio::test]
    #[serial(online_ddl)]
    async fn advisory_lock_contention_skips_cleanly() {
        let Some(url) = std::env::var("DATABASE_URL").ok() else {
            eprintln!("skipping: DATABASE_URL not set");
            return;
        };
        let mut check_conn = conn_or_skip().await.expect("connect");
        let table = "jef580_contended_test";
        let name = "jef580_contended_test_idx";
        setup_throwaway_table(&mut check_conn, table).await;

        // Hold the lane's advisory lock on a separate connection, simulating
        // another replica already running its build.
        let mut holder = PgConnection::connect(&url).await.expect("connect holder");
        let held: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
            .bind(ADVISORY_LOCK_KEY)
            .fetch_one(&mut holder)
            .await
            .expect("take lock");
        assert!(held, "test setup: must acquire the lock itself first");

        let desired = [OnlineIndex {
            name,
            table,
            create_sql:
                "CREATE INDEX CONCURRENTLY jef580_contended_test_idx ON jef580_contended_test (id)",
        }];
        run_indexes(&url, &desired)
            .await
            .expect("a contended run must return Ok, not error");

        // If the lock were not respected, this index would now be Valid.
        assert_eq!(
            index_state(&mut check_conn, name).await.unwrap(),
            IndexState::Absent,
            "a run that lost the advisory lock must not touch any index"
        );

        sqlx::query("SELECT pg_advisory_unlock($1)")
            .bind(ADVISORY_LOCK_KEY)
            .execute(&mut holder)
            .await
            .expect("release lock");
        drop_throwaway_table(&mut check_conn, table).await;
    }

    #[tokio::test]
    #[serial(online_ddl)]
    async fn seeded_covering_index_run_is_a_noop() {
        let Some(url) = std::env::var("DATABASE_URL").ok() else {
            eprintln!("skipping: DATABASE_URL not set");
            return;
        };
        // Migrations (0017) already build this index by hand, matching prod
        // where it exists and is valid before the lane ever runs.
        db::migrate(&url).await.expect("migrate");
        let mut conn = PgConnection::connect(&url).await.expect("connect");
        let name = DESIRED_INDEXES[0].name;

        assert_eq!(
            index_state(&mut conn, name).await.unwrap(),
            IndexState::Valid,
            "migration 0017 must have already built this index"
        );
        let oid_before = relation_oid(&mut conn, name).await;

        run(&url)
            .await
            .expect("lane run against an already-valid index");

        assert_eq!(
            oid_before,
            relation_oid(&mut conn, name).await,
            "the lane's first run against the seeded prod index must be a no-op"
        );
    }
}
