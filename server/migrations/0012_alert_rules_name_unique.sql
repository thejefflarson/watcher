-- Declarative alert rules: a JSON config (rendered from the chart's values) is now
-- the source of truth, reconciled into this table on startup by upserting on the
-- rule name. That upsert needs `name` to be unique. Collapse any pre-existing
-- duplicate names (keep the most recent) before enforcing the constraint so the
-- migration can't fail on legacy hand-made rows.
DELETE FROM alert_rules a
USING alert_rules b
WHERE a.name = b.name AND a.id < b.id;

CREATE UNIQUE INDEX IF NOT EXISTS alert_rules_name_key ON alert_rules (name);
