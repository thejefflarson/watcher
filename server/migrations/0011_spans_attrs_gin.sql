-- GIN index on span attributes so the traces list can filter by attribute
-- (attributes @> '{"key":"value"}') without seq-scanning the spans table.
-- Mirrors logs_attrs_gin; jsonb_path_ops is compact and serves the @> operator.
--
-- SET LOCAL statement_timeout = 0 for this migration only: the pool sets a 60s
-- statement_timeout, but building a GIN over the existing spans can take longer,
-- and we don't want the build aborted (which would crash-loop startup). The build
-- holds a write lock on spans, so ingest pauses briefly once while it runs.
SET LOCAL statement_timeout = 0;
CREATE INDEX IF NOT EXISTS spans_attrs_gin ON spans USING gin (attributes jsonb_path_ops);
