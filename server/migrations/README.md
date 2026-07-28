# Migrations

`sqlx::migrate!` embeds this directory into the server binary and runs every
migration, in order, on startup — including on every pod boot in production.
Add a new numbered file for a schema change; **never edit an already-applied
one**. sqlx checksums each applied migration against what's recorded in its
`_sqlx_migrations` table, so editing a file that has already run in prod makes
the next boot's checksum check fail and crashloops the deployment.

## Heavy/online DDL on the big telemetry tables

`spans`, `logs`, and `metric_series_rollups` are the high-row-count,
continuously-written tables. A boot migration that runs plain (non-`CONCURRENTLY`)
`CREATE`/`DROP INDEX`, or an `ADD COLUMN ... NOT NULL DEFAULT` rewrite, against
one of them briefly blocks writes to that table — fine for a genuinely small or
brand-new table, a real stall risk once one of these three has millions of rows
(see `0017_metric_series_rollups_covering_idx.sql`, which hit exactly this).
Online/heavy DDL against `spans`, `logs`, or `metric_series_rollups` belongs in
the JEF-580 online-DDL lane (ADR 0021) — run out-of-band from `sqlx::migrate!`'s
advisory lock, not a boot migration.

## `-- no-transaction` migrations: one statement per file

sqlx runs a migration marked `-- no-transaction` outside its wrapping
transaction (needed for `CREATE INDEX CONCURRENTLY`, which cannot run inside
one). It only supports **one statement per no-transaction file** — split a
multi-step no-transaction change across several numbered migrations instead.

## CI lint: `scripts/lint-migrations.sh` (JEF-592)

CI runs `scripts/lint-migrations.sh` against every file in this directory and
fails the build on:

1. `CREATE INDEX` / `DROP INDEX` without `CONCURRENTLY`.
2. `ALTER TABLE ... ADD COLUMN ... NOT NULL DEFAULT ...` (table rewrite).
3. A multi-statement file that also carries a `-- no-transaction` comment.

**Escape hatch:** add a `-- lock-ok:<reason>` comment anywhere in the file if
the flagged statement is genuinely safe — e.g. an index built on a table
created earlier in the same migration, which is still empty. Don't reach for
this on `spans`/`logs`/`metric_series_rollups`; use the online-DDL lane above
instead.

**Grandfathering:** the lint only checks migrations numbered *above* the
baseline recorded in `.locklint-baseline` (currently `17`) — migrations at or
below it predate the lint and are exempt without being edited, since editing an
applied migration (see above) would crashloop prod. Do not bump the baseline to
grandfather a *new* migration; it only ever records the version of the lint's
introduction.
