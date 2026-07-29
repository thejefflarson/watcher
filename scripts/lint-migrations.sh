#!/bin/sh
# CI guardrail: fail the build on migration DDL that can stall the big
# telemetry tables (spans, logs, metric_series_rollups) at deploy time.
#
# Migration 0017 shipped a plain (non-CONCURRENTLY) `CREATE INDEX` on
# metric_series_rollups -- at the time a 10.4M-row hot table -- and nothing in
# CI flagged it as risky. That specific build was a deliberate, documented
# trade-off (see the comment in 0017_metric_series_rollups_covering_idx.sql:
# sqlx::migrate! holds a session-level advisory lock for the whole run, and
# ingest's concurrent upserts can deadlock a CONCURRENTLY build against that
# lock), but the next one might not be -- this lint makes that a conscious,
# reviewed choice instead of a silent default.
#
# Rules, applied to every *.sql file in the migrations dir:
#   1. `CREATE INDEX` / `DROP INDEX` without `CONCURRENTLY`.
#   2. `ALTER TABLE ... ADD COLUMN ... NOT NULL DEFAULT ...` (table rewrite to
#      backfill the default on every existing row).
#   3. A multi-statement file that ALSO carries a `-- no-transaction` comment
#      (sqlx only supports one statement per no-transaction migration).
#
# Escape hatch: a `-- lock-ok:<reason>` comment anywhere in the file allows it
# (e.g. an index built on a table created earlier in the SAME migration is
# instant -- there's no data in it yet).
#
# Grandfathering (CRITICAL -- do not edit already-applied migrations): sqlx
# checksums every applied migration, so editing an old file to add a
# `-- lock-ok` comment crashloops prod on next boot. Instead, migrations at or
# below the baseline version recorded in `<migrations-dir>/.locklint-baseline`
# are skipped entirely, unedited. Only migrations ADDED after the baseline are
# linted -- see server/migrations/README.md.
#
# Usage:
#   scripts/lint-migrations.sh [migrations-dir]   # default: server/migrations
#   scripts/lint-migrations.sh --selftest         # run the built-in fixtures
set -u

# Leading numeric prefix of a migration filename, leading zeros stripped
# ("0008_foo.sql" -> "8"; no prefix -> "0"). Avoids `$((10#$n))`, which not
# every /bin/sh implements consistently, and plain arithmetic on a
# zero-padded number (`018`) is invalid octal in POSIX arithmetic.
migration_version() {
    n=$(basename "$1" | sed -nE 's/^([0-9]+)_.*\.sql$/\1/p')
    n=$(printf '%s' "$n" | sed 's/^0*//')
    [ -z "$n" ] && n="0"
    printf '%s' "$n"
}

# One pass over $1: strips `--` comments, splits on `;`, and for each
# non-empty statement prints "LOCK:<stmt>" or "REWRITE:<stmt>" per violation,
# then a trailing "COUNT:<n>" of non-empty statements (rule 3's `-- no-
# transaction` marker itself is checked separately, against the raw file).
lint_file_statements() {
    awk '
        {
            line = $0
            sub(/--.*/, "", line)
            buf = buf " " line "\n"
        }
        END {
            n = split(buf, stmts, ";")
            count = 0
            for (i = 1; i <= n; i++) {
                s = stmts[i]
                gsub(/[ \t\n]+/, " ", s)
                sub(/^ /, "", s)
                sub(/ $/, "", s)
                if (s == "") continue
                count++
                ls = tolower(s)
                if (ls ~ /^(create( unique)? index|drop index)( |$)/ && ls !~ /concurrently/) {
                    print "LOCK:" s
                }
                if (ls ~ /alter table/ && ls ~ /add column/ && ls ~ /not null/ && ls ~ /default/) {
                    print "REWRITE:" s
                }
            }
            print "COUNT:" count
        }
    ' "$1"
}

# Runs every rule against every *.sql file strictly newer than $2's baseline;
# prints one "file: message" line per violation to stdout. Returns the
# violation count.
lint_dir() {
    dir="$1"
    baseline_file="$2"
    baseline=0
    if [ -f "$baseline_file" ]; then
        baseline=$(sed 's/^0*//' "$baseline_file" | tr -d '[:space:]')
        [ -z "$baseline" ] && baseline=0
    fi

    violations=0
    for f in "$dir"/*.sql; do
        [ -e "$f" ] || continue
        v=$(migration_version "$f")
        if [ "$v" -le "$baseline" ] 2>/dev/null; then
            continue
        fi

        if grep -qi -- '-- lock-ok:' "$f"; then
            continue
        fi

        stmt_count=0
        while IFS= read -r hit; do
            [ -z "$hit" ] && continue
            case "$hit" in
                LOCK:*)
                    echo "$f: non-CONCURRENTLY CREATE/DROP INDEX (table-locking DDL): ${hit#LOCK:}"
                    violations=$((violations + 1))
                    ;;
                REWRITE:*)
                    echo "$f: ADD COLUMN ... NOT NULL DEFAULT (table rewrite): ${hit#REWRITE:}"
                    violations=$((violations + 1))
                    ;;
                COUNT:*)
                    stmt_count="${hit#COUNT:}"
                    ;;
            esac
        done <<EOF
$(lint_file_statements "$f")
EOF

        if [ "$stmt_count" -gt 1 ] && grep -qi -- '-- no-transaction' "$f"; then
            echo "$f: -- no-transaction file with ${stmt_count} statements (sqlx allows exactly one)"
            violations=$((violations + 1))
        fi
    done
    return "$violations"
}

run_lint() {
    dir="${1:-server/migrations}"
    baseline_file="${LOCKLINT_BASELINE_FILE:-${dir}/.locklint-baseline}"
    out=$(lint_dir "$dir" "$baseline_file")
    status=$?
    if [ -n "$out" ]; then
        printf '%s\n' "$out" >&2
    fi
    if [ "$status" -ne 0 ]; then
        echo "lint-migrations: ${status} violation(s) found" >&2
        return 1
    fi
    echo "lint-migrations: clean (checked $(basename "$dir")/*.sql above baseline $(cat "$baseline_file" 2>/dev/null || echo 0))"
    return 0
}

# -- Self-test ---------------------------------------------------------------
# Proves the lint actually catches each risky pattern (and doesn't false-flag
# an escaped/grandfathered/safe one) using throwaway fixture files, so a
# regression here is caught in CI in seconds rather than by the next migration
# that reaches prod unflagged.
selftest() {
    tmp=$(mktemp -d)
    trap 'rm -rf "${tmp}"' EXIT
    fails=0

    printf '5\n' >"${tmp}/.locklint-baseline"

    # 1. Grandfathered: version <= baseline, would otherwise trip rule 1.
    cat >"${tmp}/0003_old_plain_index.sql" <<'SQL'
CREATE INDEX IF NOT EXISTS old_idx ON widgets (name);
SQL

    # 2. New migration, plain CREATE INDEX, no escape -> MUST fail.
    cat >"${tmp}/0006_bad_plain_index.sql" <<'SQL'
CREATE INDEX IF NOT EXISTS widgets_name_idx ON widgets (name);
SQL

    # 3. New migration, CONCURRENTLY -> clean.
    cat >"${tmp}/0007_good_concurrent_index.sql" <<'SQL'
CREATE INDEX CONCURRENTLY IF NOT EXISTS widgets_kind_idx ON widgets (kind);
SQL

    # 4. New migration, plain CREATE INDEX but escaped -> clean.
    cat >"${tmp}/0008_escaped_index.sql" <<'SQL'
-- lock-ok: widgets is created earlier in this same migration and is empty.
CREATE TABLE widgets (id BIGSERIAL PRIMARY KEY, name TEXT);
CREATE INDEX IF NOT EXISTS widgets_name2_idx ON widgets (name);
SQL

    # 5. New migration, ADD COLUMN ... NOT NULL DEFAULT -> MUST fail.
    cat >"${tmp}/0009_bad_rewrite.sql" <<'SQL'
ALTER TABLE widgets ADD COLUMN active BOOLEAN NOT NULL DEFAULT true;
SQL

    # 6. New migration, nullable ADD COLUMN -> clean.
    cat >"${tmp}/0010_good_add_column.sql" <<'SQL'
ALTER TABLE widgets ADD COLUMN IF NOT EXISTS note TEXT;
SQL

    # 7. New migration, no-transaction with two statements -> MUST fail.
    cat >"${tmp}/0011_bad_no_txn.sql" <<'SQL'
-- no-transaction
CREATE INDEX CONCURRENTLY IF NOT EXISTS widgets_a_idx ON widgets (a);
CREATE INDEX CONCURRENTLY IF NOT EXISTS widgets_b_idx ON widgets (b);
SQL

    # 8. New migration, no-transaction with one statement -> clean.
    cat >"${tmp}/0012_good_no_txn.sql" <<'SQL'
-- no-transaction
CREATE INDEX CONCURRENTLY IF NOT EXISTS widgets_c_idx ON widgets (c);
SQL

    out=$(lint_dir "${tmp}" "${tmp}/.locklint-baseline")
    status=$?

    expect_hit() { # expect_hit <label> <pattern>
        if printf '%s\n' "$out" | grep -q "$2"; then
            echo "  ok   $1"
        else
            echo "  FAIL $1 -- expected a hit matching /$2/ in:" >&2
            printf '%s\n' "$out" | sed 's/^/       /' >&2
            fails=$((fails + 1))
        fi
    }
    expect_clean() { # expect_clean <label> <pattern>
        if printf '%s\n' "$out" | grep -q "$2"; then
            echo "  FAIL $1 -- unexpected hit matching /$2/ in:" >&2
            printf '%s\n' "$out" | sed 's/^/       /' >&2
            fails=$((fails + 1))
        else
            echo "  ok   $1"
        fi
    }

    expect_clean "grandfathered plain index (0003, <= baseline) not flagged" "0003_old_plain_index"
    expect_hit   "new plain CREATE INDEX (0006) flagged" "0006_bad_plain_index.*non-CONCURRENTLY"
    expect_clean "CONCURRENTLY index (0007) not flagged" "0007_good_concurrent_index"
    expect_clean "lock-ok escaped index (0008) not flagged" "0008_escaped_index"
    expect_hit   "ADD COLUMN NOT NULL DEFAULT (0009) flagged" "0009_bad_rewrite.*table rewrite"
    expect_clean "nullable ADD COLUMN (0010) not flagged" "0010_good_add_column"
    expect_hit   "multi-statement no-transaction (0011) flagged" "0011_bad_no_txn.*no-transaction file with 2 statements"
    expect_clean "single-statement no-transaction (0012) not flagged" "0012_good_no_txn"

    if [ "$status" -ne 3 ]; then
        echo "  FAIL violation count -- expected 3, got ${status}" >&2
        fails=$((fails + 1))
    else
        echo "  ok   violation count == 3"
    fi

    if [ "$fails" -ne 0 ]; then
        echo "selftest: ${fails} failure(s)" >&2
        return 1
    fi
    echo "selftest: all fixtures passed"
}

if [ "${1:-}" = "--selftest" ]; then
    selftest
else
    run_lint "${1:-server/migrations}"
fi
