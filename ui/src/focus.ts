import { RANGES } from "./timerange";

// A human suffix describing the active service focus + time window, for wording
// empty states to the focus, e.g. "No logs<focusLabel>." →
// "No logs for payments in the last 1h." (or just "No logs." when unscoped/all).
export function focusLabel(service: string, rangeKey: string): string {
  const range = RANGES.find((r) => r.key === rangeKey);
  const window = range && range.ms > 0 ? ` in the last ${range.label}` : "";
  const svc = service ? ` for ${service}` : "";
  return `${svc}${window}`;
}
