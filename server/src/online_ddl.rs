//! In-app online (non-transactional) DDL lane (ADR 0021).
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
//!
//! A valid index isn't necessarily up to date, though: matching by name alone
//! (the original check) can't see a definition change, so it would treat
//! an existing valid index as done forever even after `DESIRED_INDEXES` changes
//! underneath it. The reconciler additionally compares a valid
//! index's live `INCLUDE` columns (via `pg_index`/`pg_attribute`, not
//! `pg_get_indexdef`'s text — see [`include_columns_drifted`]) against the
//! desired set, and drops + rebuilds on a mismatch.

use crate::db;
use sqlx::postgres::PgConnection;
use sqlx::Connection;

/// A single desired online index: its name, the table it's built on (for
/// logging), the `INCLUDE` columns it's expected to carry (for drift detection,
/// see [`include_columns_drifted`]), and the exact `CREATE INDEX CONCURRENTLY`
/// statement that builds it.
pub struct OnlineIndex {
    pub name: &'static str,
    pub table: &'static str,
    /// The columns the index's `INCLUDE` clause is expected to carry, used to
    /// detect a definition change against a live, valid index (which the name +
    /// validity check alone can't see — see [`include_columns_drifted`]).
    pub include_cols: &'static [&'static str],
    pub create_sql: &'static str,
}

/// The indexes this lane manages. `metric_series_rollups_name_bucket_covering_idx`
/// was originally seeded with the full column list migration 0017 built
/// by hand in production; it was later narrowed to the scalar-only `INCLUDE` list
/// below (dropping the heavy `attrs` JSONB and `bucket_bounds`/`bucket_counts`
/// arrays, which only the rarer facet/histogram read paths need — the hot
/// `query_metric_series` path only ever selects `sum`/`count`) — see
/// `include_columns_drifted`, which makes the lane detect and rebuild that
/// narrowing against the wide index migration 0017 built, rather than treating
/// the existing valid-and-same-name index as done forever.
pub const DESIRED_INDEXES: &[OnlineIndex] = &[OnlineIndex {
    name: "metric_series_rollups_name_bucket_covering_idx",
    table: "metric_series_rollups",
    include_cols: &[
        "service",
        "kind",
        "unit",
        "is_monotonic",
        "count",
        "sum",
        "avg",
        "max",
    ],
    create_sql: "CREATE INDEX CONCURRENTLY metric_series_rollups_name_bucket_covering_idx \
                 ON metric_series_rollups (name, bucket) \
                 INCLUDE (service, kind, unit, is_monotonic, count, sum, avg, max)",
}];

/// Session-level advisory-lock key that gates a whole lane run, so exactly one
/// replica builds during a rollout and the others skip cleanly. Arbitrary but
/// fixed, and deliberately NOT the key `sqlx::migrate!` uses (which it derives
/// at runtime from a hash of the database name, not a static constant), so the
/// two can never collide.
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

/// The live `INCLUDE` columns of the index named `name`, read from the
/// catalog rather than `pg_get_indexdef`'s text: `indexdef` isn't a stable
/// string to compare against our own `create_sql` (it omits `CONCURRENTLY`,
/// may schema-qualify names, and doesn't guarantee `INCLUDE` column order), so
/// this instead walks `pg_index.indkey` — which lists every indexed column,
/// key columns first — and takes everything past `indnkeyatts` (the number of
/// true key columns), which is exactly the `INCLUDE` list. Only meaningful for
/// an index that's known to exist (call after `index_state` confirms it does).
async fn live_include_columns(conn: &mut PgConnection, name: &str) -> anyhow::Result<Vec<String>> {
    let mut cols: Vec<String> = sqlx::query_scalar(
        "SELECT a.attname FROM pg_index i \
         JOIN pg_class c ON c.oid = i.indexrelid \
         JOIN LATERAL unnest(i.indkey::int2[]) WITH ORDINALITY AS k(attnum, ord) ON true \
         JOIN pg_attribute a ON a.attrelid = i.indrelid AND a.attnum = k.attnum \
         WHERE c.relname = $1 AND k.ord > i.indnkeyatts",
    )
    .bind(name)
    .fetch_all(&mut *conn)
    .await?;
    cols.sort_unstable();
    Ok(cols)
}

/// `idx.include_cols` as an owned, sorted `Vec<String>` — the same shape
/// [`live_include_columns`] returns, so the two are directly comparable.
fn desired_include_cols(idx: &OnlineIndex) -> Vec<String> {
    let mut cols: Vec<String> = idx.include_cols.iter().map(|s| s.to_string()).collect();
    cols.sort_unstable();
    cols
}

/// Whether a live, valid index named `idx.name` carries a different `INCLUDE`
/// column set than `idx.include_cols` desires — i.e. whether the definition has
/// drifted (e.g. narrowing the wide covering index migration 0017
/// built). Order-independent: only the column *set* matters.
async fn include_columns_drifted(
    conn: &mut PgConnection,
    idx: &OnlineIndex,
) -> anyhow::Result<bool> {
    let live = live_include_columns(conn, idx.name).await?;
    Ok(live != desired_include_cols(idx))
}

/// `DROP INDEX CONCURRENTLY` for `idx.name`, outside a transaction as
/// `CONCURRENTLY` requires. Shared by the invalid-index and drifted-definition
/// rebuild paths in [`reconcile_index`].
async fn drop_index_concurrently(conn: &mut PgConnection, name: &str) -> anyhow::Result<()> {
    // AssertSqlSafe: name is interpolated only from our own hardcoded
    // OnlineIndex constants (never user input), same as retention.rs's
    // table-name interpolation.
    let drop_sql = format!("DROP INDEX CONCURRENTLY {name}");
    sqlx::query(sqlx::AssertSqlSafe(drop_sql))
        .execute(conn)
        .await?;
    Ok(())
}

/// Reconciles one desired index against its current state on `conn`. Absent →
/// build; valid and matching desired `INCLUDE` columns → no-op; valid but
/// drifted (a definition change since it was built, e.g. the covering index's narrowing)
/// → drop then rebuild; invalid (left by an interrupted build) → drop then
/// rebuild. Runs entirely outside a transaction, as `CONCURRENTLY` requires.
async fn reconcile_index(conn: &mut PgConnection, idx: &OnlineIndex) -> anyhow::Result<()> {
    match index_state(conn, idx.name).await? {
        IndexState::Valid => {
            if include_columns_drifted(conn, idx).await? {
                tracing::warn!(
                    index = idx.name,
                    "online-ddl: valid but INCLUDE columns drifted from desired definition; \
                     dropping to rebuild"
                );
                drop_index_concurrently(conn, idx.name).await?;
                build_index(conn, idx).await
            } else {
                tracing::debug!(index = idx.name, "online-ddl: already valid, no-op");
                Ok(())
            }
        }
        IndexState::Absent => build_index(conn, idx).await,
        IndexState::Invalid => {
            tracing::warn!(
                index = idx.name,
                "online-ddl: exists but INVALID (interrupted build); dropping to rebuild"
            );
            drop_index_concurrently(conn, idx.name).await?;
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
            include_cols: &[],
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
            include_cols: &[],
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
            include_cols: &[],
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
            include_cols: &[],
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
    async fn seeded_covering_index_narrows_then_is_a_noop() {
        let Some(url) = std::env::var("DATABASE_URL").ok() else {
            eprintln!("skipping: DATABASE_URL not set");
            return;
        };
        db::migrate(&url).await.expect("migrate");
        let mut conn = PgConnection::connect(&url).await.expect("connect");
        let name = DESIRED_INDEXES[0].name;

        // Force the real table into the exact WIDE state migration 0017 builds
        // by hand (it can't be edited — see the module docs and ADR 0021),
        // regardless of what an earlier run of this test (against this same
        // persistent dev DB) may have already narrowed it to — so this test is
        // repeatable across runs, not just against a pristine DB.
        sqlx::query(sqlx::AssertSqlSafe(format!("DROP INDEX IF EXISTS {name}")))
            .execute(&mut conn)
            .await
            .expect("drop any existing index for test setup");
        sqlx::query(
            "CREATE INDEX metric_series_rollups_name_bucket_covering_idx \
             ON metric_series_rollups (name, bucket) \
             INCLUDE (service, kind, unit, is_monotonic, count, sum, avg, max, attrs, \
             bucket_bounds, bucket_counts)",
        )
        .execute(&mut conn)
        .await
        .expect("build wide baseline for test setup");

        assert_eq!(
            index_state(&mut conn, name).await.unwrap(),
            IndexState::Valid
        );
        let oid_wide = relation_oid(&mut conn, name).await;

        // First run: the lane must detect the wide-vs-narrow drift and
        // DROP CONCURRENTLY + rebuild to the narrow INCLUDE set.
        run(&url).await.expect("lane run narrows the wide index");

        let oid_narrow = relation_oid(&mut conn, name).await;
        assert_ne!(
            oid_wide, oid_narrow,
            "a drifted definition must be dropped and rebuilt (new oid)"
        );
        assert_eq!(
            index_state(&mut conn, name).await.unwrap(),
            IndexState::Valid
        );
        let want = desired_include_cols(&DESIRED_INDEXES[0]);
        assert_eq!(
            live_include_columns(&mut conn, name).await.unwrap(),
            want,
            "rebuilt index must carry exactly the desired narrow INCLUDE columns"
        );

        // Second run: now matching, so it must be a no-op (idempotent).
        run(&url)
            .await
            .expect("second run against a matching index");
        assert_eq!(
            oid_narrow,
            relation_oid(&mut conn, name).await,
            "a run against an already-narrowed index must be a no-op"
        );
    }

    #[tokio::test]
    #[serial(online_ddl)]
    async fn drifted_include_columns_are_dropped_and_rebuilt() {
        let Some(mut conn) = conn_or_skip().await else {
            eprintln!("skipping: DATABASE_URL not set");
            return;
        };
        let table = "jef591_drift_test";
        let name = "jef591_drift_test_idx";
        let _ = sqlx::query(sqlx::AssertSqlSafe(format!("DROP TABLE IF EXISTS {table}")))
            .execute(&mut conn)
            .await;
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "CREATE TABLE {table} (id int, a int, b int, c int)"
        )))
        .execute(&mut conn)
        .await
        .expect("create throwaway table");

        // Pre-create a WIDE index, mirroring migration 0017's untouched def.
        let wide = OnlineIndex {
            name,
            table,
            include_cols: &["a", "b", "c"],
            create_sql: "CREATE INDEX CONCURRENTLY jef591_drift_test_idx \
                         ON jef591_drift_test (id) INCLUDE (a, b, c)",
        };
        reconcile_index(&mut conn, &wide).await.expect("build wide");
        assert_eq!(
            live_include_columns(&mut conn, name).await.unwrap(),
            vec!["a".to_string(), "b".to_string(), "c".to_string()]
        );
        let oid_wide = relation_oid(&mut conn, name).await;

        // Desired is now narrow: only column `a`.
        let narrow = OnlineIndex {
            name,
            table,
            include_cols: &["a"],
            create_sql: "CREATE INDEX CONCURRENTLY jef591_drift_test_idx \
                         ON jef591_drift_test (id) INCLUDE (a)",
        };
        reconcile_index(&mut conn, &narrow)
            .await
            .expect("drop + rebuild narrow");

        let oid_narrow = relation_oid(&mut conn, name).await;
        assert_ne!(
            oid_wide, oid_narrow,
            "a drifted INCLUDE set must be dropped and rebuilt (new oid)"
        );
        assert_eq!(
            index_state(&mut conn, name).await.unwrap(),
            IndexState::Valid
        );
        assert_eq!(
            live_include_columns(&mut conn, name).await.unwrap(),
            vec!["a".to_string()]
        );

        // Re-running against the now-matching definition must be a no-op.
        reconcile_index(&mut conn, &narrow)
            .await
            .expect("second reconcile is a no-op");
        assert_eq!(
            oid_narrow,
            relation_oid(&mut conn, name).await,
            "reconcile against a matching definition must not rebuild again"
        );

        drop_throwaway_table(&mut conn, table).await;
    }
}
