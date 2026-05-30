const BASE =
  (import.meta.env.VITE_API_BASE as string | undefined) ?? "http://localhost:4318";

// Optional API token (when the server has WATCHER_API_TOKEN set).
const TOKEN_KEY = "watcher_token";
export const getToken = () => localStorage.getItem(TOKEN_KEY) ?? "";
export const setToken = (t: string) => localStorage.setItem(TOKEN_KEY, t);

export class Unauthorized extends Error {}

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

export interface MetricSummary {
  name: string;
  service: string | null;
  kind: string | null;
  unit: string | null;
  points: number;
  last_time: string;
  last_value: number | null;
}

export interface ServiceMapData {
  nodes: string[];
  edges: { source: string; target: string; calls: number }[];
}

async function get<T>(path: string): Promise<T> {
  const token = getToken();
  const res = await fetch(`${BASE}${path}`, {
    headers: token ? { authorization: `Bearer ${token}` } : {},
  });
  if (res.status === 401) throw new Unauthorized("unauthorized");
  if (!res.ok) throw new Error(`${res.status} ${res.statusText}`);
  return (await res.json()) as T;
}

function qs(params: Record<string, string | number | undefined>): string {
  const q = new URLSearchParams();
  for (const [k, v] of Object.entries(params)) {
    if (v !== undefined && v !== "") q.set(k, String(v));
  }
  return q.toString();
}

export const listTraces = (p: { service?: string; limit?: number } = {}) =>
  get<TraceSummary[]>(`/api/traces?${qs(p)}`);

export const getTrace = (traceId: string) => get<SpanRow[]>(`/api/traces/${traceId}`);

export const listLogs = (
  p: { service?: string; q?: string; trace_id?: string; limit?: number } = {},
) => get<LogRow[]>(`/api/logs?${qs(p)}`);

export const listMetrics = (p: { service?: string; limit?: number } = {}) =>
  get<MetricSummary[]>(`/api/metrics?${qs(p)}`);

export const getServiceMap = () => get<ServiceMapData>(`/api/servicemap`);
