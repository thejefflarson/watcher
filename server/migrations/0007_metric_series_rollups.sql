-- Per-series downsampled rollups: like metric_rollups, but keyed by the full
-- series identity (attributes) instead of collapsing it away. This is what makes
-- the faceted views and the expandable list cheap — they read these downsampled
-- rows instead of scanning raw points per request — and it lets per-series
-- history survive past the short raw-metric retention window.

-- Element-wise sum of two equal-length bigint arrays (NULL-padded), so histogram
-- bucket_counts can be aggregated across a series' points in a time bucket.
CREATE OR REPLACE FUNCTION array_add(a bigint[], b bigint[]) RETURNS bigint[]
    LANGUAGE sql IMMUTABLE AS $$
    SELECT array_agg(coalesce(x, 0) + coalesce(y, 0) ORDER BY ord)
    FROM unnest(a, b) WITH ORDINALITY AS u(x, y, ord)
$$;

DROP AGGREGATE IF EXISTS array_sum(bigint[]);
CREATE AGGREGATE array_sum(bigint[]) (
    sfunc = array_add,
    stype = bigint[],
    initcond = '{}'
);

CREATE TABLE IF NOT EXISTS metric_series_rollups (
    bucket        TIMESTAMPTZ      NOT NULL,
    name          TEXT             NOT NULL,
    -- stable identity for the series' attribute set (the upsert key).
    series_key    TEXT             NOT NULL,
    attrs         JSONB            NOT NULL DEFAULT '{}',
    service       TEXT,
    kind          TEXT,
    unit          TEXT,
    is_monotonic  BOOLEAN,
    count         BIGINT           NOT NULL,
    sum           DOUBLE PRECISION,
    min           DOUBLE PRECISION,
    max           DOUBLE PRECISION,            -- also the cumulative level for counters
    avg           DOUBLE PRECISION,
    bucket_bounds DOUBLE PRECISION[],          -- histogram bounds (shared per series)
    bucket_counts BIGINT[],                    -- histogram counts, summed over the bucket
    PRIMARY KEY (name, series_key, bucket)
);

CREATE INDEX IF NOT EXISTS metric_series_rollups_name_bucket_idx
    ON metric_series_rollups (name, bucket DESC);
