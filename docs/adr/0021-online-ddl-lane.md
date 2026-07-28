# 0021. Online (non-transactional) schema changes run in an in-app DDL lane

- Status: Accepted
- Date: 2026-07-27
- Relates to: [0020](0020-on-ingest-per-series-metric-rollups.md)

## Context

Migrations run via `sqlx::migrate!("./migrations").run(pool)` on every pod boot
(`server/src/db.rs`), **before** the HTTP listener binds (`server/src/main.rs`) — so a
migration failure exits the process before `/healthz` is served, Kubernetes restarts
it, and the same DDL runs again.

`sqlx::migrate!` wraps each migration in a transaction and holds a session-level
advisory lock for the whole run. That makes it the wrong place for heavy or
non-transactional DDL, and we hit **both** failure modes on `metric_series_rollups`
(~10.4M rows / 5.8 GB in production):

- **JEF-548 (first attempt):** `CREATE INDEX CONCURRENTLY` cannot run inside a
  transaction and deadlocked against the migrator's advisory lock.
- **JEF-580 (second attempt):** a plain, transactional `CREATE INDEX` took a `SHARE`
  lock on the table and ran past the pool's 60 s `statement_timeout`, so every boot's
  migration was cancelled → CrashLoopBackOff, and each attempt re-locked the table and
  stalled cluster-wide OTLP ingest for ~25 minutes until the index was built by hand
  out-of-band with `CREATE INDEX CONCURRENTLY`.

The constraints rule out the obvious alternatives:

- A **pre-deploy migration Job / ArgoCD PreSync hook** couples every schema change to
  the separate `../cluster` GitOps repo, fires on every continuous-deploy digest bump
  (heavy on a Pi), and adds a failure surface that must still coordinate with the
  on-boot migrator.
- Merely **raising `statement_timeout` + adding `lock_timeout`** on the migrate
  connection cures the crashloop but not the stall — a plain `CREATE INDEX` still holds
  its lock for the whole multi-minute build.
- Migration `0017` is already checksum-recorded as applied, so it **cannot be edited**
  (that fails `sqlx::migrate!`'s content check and crashloops again); corrective DDL
  must land as new steps.

## Decision

We will run **online (non-transactional) DDL** — `CREATE INDEX CONCURRENTLY`,
`DROP INDEX CONCURRENTLY`, and future online changes — from an **in-app online-DDL
lane**, separate from the transactional `sqlx::migrate!` migrator:

- A declarative list of desired online-index definitions in the server binary
  (e.g. `server/src/online_ddl.rs`).
- Executed **after** `db::migrate()` returns (its advisory lock released) **and after
  the HTTP listener binds** (the pod is already Ready), in a spawned background task —
  so boot never blocks on an index build.
- On a **dedicated connection** (not the shared query pool, which carries the app's
  60 s `statement_timeout`), with session `statement_timeout = 0` and a small bounded
  `lock_timeout`, issued outside any transaction (as `CONCURRENTLY` requires).
- Guarded by its **own `pg_try_advisory_lock` key** (distinct from sqlx's) so exactly
  one replica builds during a rollout and the others skip.
- **Idempotent and INVALID-index-safe:** the runner checks `pg_index.indisvalid`
  rather than relying on `IF NOT EXISTS` (which treats a left-behind INVALID index as
  present forever); an invalid target is `DROP INDEX CONCURRENTLY`-ed and rebuilt, so
  an interrupted build self-heals on the next boot.

Ordinary transactional DDL stays in `sqlx::migrate!` and must stay fast. **Policy:
heavy, locking, or non-transactional DDL goes to the online lane; transactional
migrations stay short.** A CI guardrail rejects plain (non-`CONCURRENTLY`) index DDL in
`server/migrations/` so the rule cannot be violated by accident.

## Consequences

- No cross-repo migration Job and no new cluster infrastructure — the mechanism ships
  in the existing server image; the deploy path stays "push image → ArgoCD rolls it".
- Boot never blocks on an index build, and ingest never stalls behind a DDL lock — the
  two failure modes above become impossible for online DDL.
- A newly-added index is briefly **absent** immediately after a deploy until the
  background build finishes; reads use the existing fallback index meanwhile (slower,
  not broken).
- A pod killed mid-build leaves an **INVALID** index; the next boot detects and
  rebuilds it — bounded, self-healing churn.
- The online lane is eventually-consistent, not transactional: an index's existence is
  not guaranteed the instant a deploy completes. Acceptable for performance indexes;
  schema that is genuinely required-for-correctness still goes through the transactional
  migrator (on small/empty tables, where a plain build is instant).
