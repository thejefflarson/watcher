-- Drop the legacy collapsed-rollup table. It was written only by the downsample
-- sweep (rollup.rs), which was removed once per-series rollups became
-- aggregate-on-insert (metric_series_rollups). Nothing writes it anymore, and
-- the metric_series read path now derives the collapsed view from
-- metric_series_rollups, so this table is dead.
DROP TABLE IF EXISTS metric_rollups;
