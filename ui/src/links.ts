// Deep-link builders shared by every cross-signal drill-in (trace ↔ logs ↔
// metrics). Trace/span ids come from ingested OTLP data — attacker-shapeable —
// so every id here is either `encodeURIComponent`-ed as a path segment or set
// via URLSearchParams: a stray `&`/`#`/`?` must stay inside its own param,
// never land on an unexpected route or inject an extra one (JEF-534).

// `/traces/:traceId`, optionally scoped to a span and/or the global service
// focus. Used by the trace list row link, a log row's drill-in, and a metric
// chart exemplar click.
export function traceHref(
  traceId: string,
  opts: { spanId?: string | null; service?: string | null } = {},
): string {
  const p = new URLSearchParams();
  if (opts.spanId) p.set("span", opts.spanId);
  if (opts.service) p.set("service", opts.service);
  const q = p.toString();
  return `/traces/${encodeURIComponent(traceId)}${q ? `?${q}` : ""}`;
}

// `/logs`, scoped to a trace and (optionally) a span.
export function logsHref(traceId: string, spanId?: string): string {
  const p = new URLSearchParams({ trace_id: traceId });
  if (spanId) p.set("span_id", spanId);
  return `/logs?${p.toString()}`;
}
