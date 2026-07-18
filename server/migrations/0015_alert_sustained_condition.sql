-- JEF-428: sustained-condition alerts (`for: 5m`). A rule may require its
-- condition to hold continuously for `for_secs` before it pages, so a one-off
-- spike (a GC pause, a deploy blip, an HA failover) no longer flaps a page.
--
--   alert_rules.for_secs   — dwell time before firing; NULL/0 = fire on the first
--                            breach (unchanged single-breach behavior).
--   alert_events.active_at — when a pending event actually fired. NULL while the
--                            condition is breaching but has not yet held for
--                            for_secs; set (and only then notified) at activation.
--
-- Pending/"breaching-since" state lives here in alert_events, never on the
-- declarative alert_rules table — mutable runtime state would fight reconcile's
-- upsert (see ADR 0015). The existing partial unique index (one open event per
-- rule, WHERE resolved_at IS NULL) carries the pending row too, so no new index
-- is needed.
ALTER TABLE alert_rules
    ADD COLUMN IF NOT EXISTS for_secs INTEGER;

ALTER TABLE alert_events
    ADD COLUMN IF NOT EXISTS active_at TIMESTAMPTZ;

-- Every event that predates this migration fired the instant it opened (there was
-- no dwell window), so backfill active_at = fired_at. This keeps the "firing"
-- read predicate (active_at IS NOT NULL) true for any already-open event.
UPDATE alert_events SET active_at = fired_at WHERE active_at IS NULL;
