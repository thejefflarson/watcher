-- Aggressive per-table autovacuum for the high-churn telemetry tables. The 7-day
-- retention prune deletes rows fine, but at Postgres' default 20% vacuum scale
-- factor autovacuum effectively never fired on logs, metric_series_rollups, and
-- spans — their steady insert+delete churn never crossed the 20%-of-table
-- threshold before the table itself had grown, so dead tuples were never
-- reclaimed and the DB ballooned to 77 GB of bloat. Lowering the scale factor to
-- 2% (and matching the insert-triggered vacuum) makes autovacuum run often enough
-- to keep up with the prune's delete churn, and the raised cost limit lets each
-- run actually make progress instead of throttling to a crawl. These were applied
-- live via ALTER TABLE; this migration makes that tuning durable across a DB
-- recreate.
ALTER TABLE logs SET (
    autovacuum_vacuum_scale_factor = 0.02,
    autovacuum_vacuum_cost_limit = 2000,
    autovacuum_vacuum_insert_scale_factor = 0.02
);

ALTER TABLE metric_series_rollups SET (
    autovacuum_vacuum_scale_factor = 0.02,
    autovacuum_vacuum_cost_limit = 2000,
    autovacuum_vacuum_insert_scale_factor = 0.02
);

ALTER TABLE spans SET (
    autovacuum_vacuum_scale_factor = 0.02,
    autovacuum_vacuum_cost_limit = 2000,
    autovacuum_vacuum_insert_scale_factor = 0.02
);
