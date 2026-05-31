-- Downsampling: pre-aggregated metric buckets so old data stays queryable after
-- the raw points are pruned. The rollup job (rollup.rs) folds raw `metrics` into
-- fixed-width time buckets; retention then keeps raw points for a short window
-- and rollups for the full retention window.
CREATE TABLE IF NOT EXISTS metric_rollups (
    bucket      TIMESTAMPTZ      NOT NULL,            -- start of the time bucket
    service     TEXT,
    -- service is nullable, but the upsert key can't be; normalise NULL -> ''.
    service_key TEXT GENERATED ALWAYS AS (coalesce(service, '')) STORED,
    name        TEXT             NOT NULL,
    kind        TEXT,
    unit        TEXT,
    count       BIGINT           NOT NULL,            -- raw points in the bucket
    sum         DOUBLE PRECISION,
    min         DOUBLE PRECISION,
    max         DOUBLE PRECISION,
    avg         DOUBLE PRECISION
);

CREATE UNIQUE INDEX IF NOT EXISTS metric_rollups_key
    ON metric_rollups (name, service_key, bucket);
CREATE INDEX IF NOT EXISTS metric_rollups_bucket_idx
    ON metric_rollups (bucket DESC);
