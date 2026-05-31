-- Query-supporting indexes. The single-column indexes from 0001/0002 don't serve
-- the API's filtered/aggregated reads well (name+time, service+time, attribute
-- equality, the service-map self-join), so those were sequential-scanning as data
-- grew. Add composites + a GIN; drop the single-column indexes the composites
-- supersede so ingest cost stays roughly flat.

-- Metrics: series, dims, grouped, alert evaluation, and the summary all filter by
-- name and a time window. A composite beats the separate name/time scans.
CREATE INDEX IF NOT EXISTS metrics_name_time_idx ON metrics (name, time DESC);
DROP INDEX IF EXISTS metrics_name_idx;

-- Spans: service-filtered recency (trace list, RED analytics)…
CREATE INDEX IF NOT EXISTS spans_service_start_idx ON spans (service, start_time DESC);
DROP INDEX IF EXISTS spans_service_idx;
-- …and the service-map self-join (child.parent_span_id -> parent.span_id within a trace).
CREATE INDEX IF NOT EXISTS spans_parent_idx ON spans (trace_id, parent_span_id);

-- Logs: service-filtered recency, and attribute-equality filtering (attr=key=value),
-- which the query now expresses as JSONB containment so this GIN serves it.
CREATE INDEX IF NOT EXISTS logs_service_time_idx ON logs (service, time DESC);
DROP INDEX IF EXISTS logs_service_idx;
CREATE INDEX IF NOT EXISTS logs_attrs_gin ON logs USING gin (attributes jsonb_path_ops);
