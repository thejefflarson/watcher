-- Shared identity expressions for the per-series rollup system. The bucket-floor
-- and series-key formulas were hand-copied across the ingest path (otlp.rs) and
-- the read path (api.rs); a single definition here keeps them from drifting (if
-- the bucketing or key ever changes, ingest-written rollups must still line up
-- with what reads compute).

-- Floor a timestamp to the start of its rollup bucket of `width` seconds.
-- STABLE (not IMMUTABLE): extract() on timestamptz is conservatively stable; we
-- never index on this, so STABLE is the correct, safe marking.
CREATE OR REPLACE FUNCTION metric_bucket(t timestamptz, width double precision)
    RETURNS timestamptz
    LANGUAGE sql
    STABLE
    PARALLEL SAFE
AS $$
    SELECT to_timestamp(floor(extract(epoch FROM t) / width) * width)
$$;

-- Stable identity for a metric series: its service plus its attribute set. This
-- is the upsert key for metric_series_rollups.
CREATE OR REPLACE FUNCTION metric_series_key(service text, attrs jsonb)
    RETURNS text
    LANGUAGE sql
    IMMUTABLE
    PARALLEL SAFE
AS $$
    SELECT md5(coalesce(service, '') || '|' || attrs::text)
$$;
