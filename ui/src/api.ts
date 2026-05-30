const BASE =
  (import.meta.env.VITE_API_BASE as string | undefined) ?? "http://localhost:4318";

export interface TraceSummary {
  trace_id: string;
  service: string | null;
  root_name: string | null;
  start_time: string;
  end_time: string;
  duration_ms: number;
  span_count: number;
  error_count: number;
}

export interface SpanRow {
  trace_id: string;
  span_id: string;
  parent_span_id: string | null;
  service: string | null;
  name: string;
  kind: number | null;
  start_time: string;
  end_time: string;
  duration_ms: number;
  status_code: number | null;
  status_message: string | null;
  attributes: Record<string, unknown>;
}

export interface LogRow {
  id: number;
  time: string;
  trace_id: string | null;
  span_id: string | null;
  service: string | null;
  severity_number: number | null;
  severity_text: string | null;
  body: string | null;
  attributes: Record<string, unknown>;
}

async function get<T>(path: string): Promise<T> {
  const res = await fetch(`${BASE}${path}`);
  if (!res.ok) throw new Error(`${res.status} ${res.statusText}`);
  return (await res.json()) as T;
}

export function listTraces(params: { service?: string; limit?: number } = {}) {
  const q = new URLSearchParams();
  if (params.service) q.set("service", params.service);
  if (params.limit) q.set("limit", String(params.limit));
  return get<TraceSummary[]>(`/api/traces?${q.toString()}`);
}

export function getTrace(traceId: string) {
  return get<SpanRow[]>(`/api/traces/${traceId}`);
}

export function listLogs(
  params: { service?: string; q?: string; trace_id?: string; limit?: number } = {},
) {
  const qs = new URLSearchParams();
  if (params.service) qs.set("service", params.service);
  if (params.q) qs.set("q", params.q);
  if (params.trace_id) qs.set("trace_id", params.trace_id);
  if (params.limit) qs.set("limit", String(params.limit));
  return get<LogRow[]>(`/api/logs?${qs.toString()}`);
}
