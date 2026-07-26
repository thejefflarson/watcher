-- Metric->trace correlation (JEF-433): keep one exemplar trace/span id per raw
-- metric point, so a spike on a chart can link straight to the trace behind it.
-- Nullable, on the existing `metrics` row -- no new table, no new retention policy
-- (ADR 0003/0011 stand): exemplars live only as long as the raw point does, and
-- are dropped (like every other per-point column) once `metric_series_rollups`
-- absorbs the point and raw retention prunes it.
ALTER TABLE metrics
    ADD COLUMN IF NOT EXISTS exemplar_trace_id TEXT,
    ADD COLUMN IF NOT EXISTS exemplar_span_id  TEXT;
