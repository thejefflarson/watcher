-- Keep histogram bucket boundaries + counts so the API can compute percentiles
-- (histogram_quantile-style linear interpolation) and render heatmaps, and
-- record whether a Sum is monotonic (a counter → show a per-second rate) versus
-- an UpDownCounter (gauge-like → show the value). All nullable: gauges set none,
-- sums set is_monotonic, histograms set the bucket arrays.
ALTER TABLE metrics
    ADD COLUMN IF NOT EXISTS is_monotonic  BOOLEAN,
    -- explicit upper bounds (OTLP explicit_bounds); length = bucket_counts - 1.
    ADD COLUMN IF NOT EXISTS bucket_bounds DOUBLE PRECISION[],
    -- per-bucket observation counts (OTLP bucket_counts); last entry is +Inf.
    ADD COLUMN IF NOT EXISTS bucket_counts BIGINT[];
