-- Drop the redundant single-column metrics(service) index. It's a leftover from
-- 0002 that 0005's composite work didn't clean up. No raw-metrics read filters by
-- service alone — service is always combined with name (e.g. `WHERE name=$1 AND
-- service=$2`), which metrics_name_time_idx already serves on its leading column.
-- On the live DB it showed 0 scans while costing ~129 MB of index maintenance on
-- every raw insert (~248 points/sec), so it's pure ingest write-amplification.
DROP INDEX IF EXISTS metrics_service_idx;
