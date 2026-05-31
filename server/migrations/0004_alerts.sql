-- Alerting: threshold rules evaluated against recent metric points, plus a log
-- of firing/resolved transitions. An event with resolved_at IS NULL is the rule
-- currently firing.
CREATE TABLE IF NOT EXISTS alert_rules (
    id          BIGSERIAL PRIMARY KEY,
    name        TEXT             NOT NULL,
    metric      TEXT             NOT NULL,           -- metric name to watch
    service     TEXT,                                -- NULL = any service
    comparator  TEXT             NOT NULL,           -- 'gt' | 'lt'
    threshold   DOUBLE PRECISION NOT NULL,
    agg         TEXT             NOT NULL DEFAULT 'avg',  -- avg|max|min|sum|last
    window_secs INTEGER          NOT NULL DEFAULT 300,    -- look-back window
    enabled     BOOLEAN          NOT NULL DEFAULT TRUE,
    created_at  TIMESTAMPTZ      NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS alert_events (
    id          BIGSERIAL PRIMARY KEY,
    rule_id     BIGINT           NOT NULL REFERENCES alert_rules (id) ON DELETE CASCADE,
    value       DOUBLE PRECISION,                    -- aggregated value at fire time
    fired_at    TIMESTAMPTZ      NOT NULL DEFAULT now(),
    resolved_at TIMESTAMPTZ                          -- NULL while still firing
);
CREATE INDEX IF NOT EXISTS alert_events_rule_idx ON alert_events (rule_id, fired_at DESC);
-- At most one open (firing) event per rule.
CREATE UNIQUE INDEX IF NOT EXISTS alert_events_open
    ON alert_events (rule_id) WHERE resolved_at IS NULL;
