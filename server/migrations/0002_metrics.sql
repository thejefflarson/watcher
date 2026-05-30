-- Third pillar: metrics. One row per data point (gauge/sum value, or histogram sum+count).
CREATE TABLE IF NOT EXISTS metrics (
    id          BIGSERIAL PRIMARY KEY,
    time        TIMESTAMPTZ NOT NULL,
    service     TEXT,
    name        TEXT        NOT NULL,
    kind        TEXT        NOT NULL,         -- gauge | sum | histogram
    value       DOUBLE PRECISION,            -- gauge/sum value, or histogram sum
    count       BIGINT,                      -- histogram count
    unit        TEXT,
    attributes  JSONB       NOT NULL DEFAULT '{}',
    ingested_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS metrics_time_idx    ON metrics (time DESC);
CREATE INDEX IF NOT EXISTS metrics_name_idx    ON metrics (name);
CREATE INDEX IF NOT EXISTS metrics_service_idx ON metrics (service);
