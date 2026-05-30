-- watcher schema: spans + logs, Postgres-native.
-- Plain Postgres tables so this runs anywhere (incl. the homelab postgres-operator).
-- If TimescaleDB is available you can later: SELECT create_hypertable('spans','start_time');

CREATE TABLE IF NOT EXISTS spans (
    id              BIGSERIAL PRIMARY KEY,
    trace_id        TEXT        NOT NULL,
    span_id         TEXT        NOT NULL,
    parent_span_id  TEXT,
    service         TEXT,
    name            TEXT        NOT NULL,
    kind            INT,
    start_time      TIMESTAMPTZ NOT NULL,
    end_time        TIMESTAMPTZ NOT NULL,
    duration_ms     DOUBLE PRECISION NOT NULL,
    status_code     INT,
    status_message  TEXT,
    attributes      JSONB       NOT NULL DEFAULT '{}',
    ingested_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (trace_id, span_id)
);
CREATE INDEX IF NOT EXISTS spans_trace_id_idx  ON spans (trace_id);
CREATE INDEX IF NOT EXISTS spans_start_time_idx ON spans (start_time DESC);
CREATE INDEX IF NOT EXISTS spans_service_idx    ON spans (service);

CREATE TABLE IF NOT EXISTS logs (
    id              BIGSERIAL PRIMARY KEY,
    time            TIMESTAMPTZ NOT NULL,
    trace_id        TEXT,
    span_id         TEXT,
    service         TEXT,
    severity_number INT,
    severity_text   TEXT,
    body            TEXT,
    attributes      JSONB       NOT NULL DEFAULT '{}',
    ingested_at     TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS logs_time_idx     ON logs (time DESC);
CREATE INDEX IF NOT EXISTS logs_trace_id_idx ON logs (trace_id);
CREATE INDEX IF NOT EXISTS logs_service_idx  ON logs (service);
