-- JEF-426: richer alert-rule scoping. Two JSONB attribute predicates plus a rate
-- flag let a rule target one specific series and evaluate a cumulative counter as
-- a per-second rate instead of aggregating its (meaningless) raw level.
--   match_attrs   — series MUST contain these attrs (attributes @> match_attrs)
--   exclude_attrs — series must NOT contain these  (NOT attributes @> exclude_attrs)
--   rate          — tri-state: NULL = auto (on for monotonic sums), TRUE/FALSE = explicit
-- All nullable so rules declared before this migration keep behaving exactly as
-- before: no predicate, no rate. The predicate is a cheap residual filter after
-- the (name, time) narrowing in alerts.rs evaluate(), so no metrics GIN is added.
ALTER TABLE alert_rules
    ADD COLUMN IF NOT EXISTS match_attrs   JSONB,
    ADD COLUMN IF NOT EXISTS exclude_attrs JSONB,
    ADD COLUMN IF NOT EXISTS rate          BOOLEAN;
