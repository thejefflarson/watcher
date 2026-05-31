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
  spark: number[] | null;
}

export interface ServiceMapData {
  nodes: string[];
  edges: { source: string; target: string; calls: number }[];
}

function authHeaders(extra?: Record<string, string>): Record<string, string> {
  const token = getToken();
  return {
    ...(token ? { authorization: `Bearer ${token}` } : {}),
    ...extra,
  };
}

async function get<T>(path: string): Promise<T> {
  const res = await fetch(`${BASE}${path}`, { headers: authHeaders() });
  if (res.status === 401) throw new Unauthorized("unauthorized");
  if (!res.ok) throw new Error(`${res.status} ${res.statusText}`);
  return (await res.json()) as T;
}

async function send<T>(method: string, path: string, body?: unknown): Promise<T | null> {
  const res = await fetch(`${BASE}${path}`, {
    method,
    headers: authHeaders(body !== undefined ? { "content-type": "application/json" } : {}),
    body: body !== undefined ? JSON.stringify(body) : undefined,
  });
  if (res.status === 401) throw new Unauthorized("unauthorized");
  if (!res.ok) throw new Error(`${res.status} ${(await res.text()) || res.statusText}`);
  // 204 No Content (DELETE) has no body to parse.
  if (res.status === 204) return null;
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

export interface SeriesPoint {
  t: string;
  v: number | null;
}

export const getMetricSeries = (p: { name: string; service?: string; hours?: number }) =>
  get<SeriesPoint[]>(`/api/metrics/series?${qs(p)}`);

export const getServiceMap = () => get<ServiceMapData>(`/api/servicemap`);

// --- Alerts ---------------------------------------------------------------

export type Comparator = "gt" | "lt";
export type Agg = "avg" | "max" | "min" | "sum" | "last";

export interface AlertRule {
  id: number;
  name: string;
  metric: string;
  service: string | null;
  comparator: Comparator;
  threshold: number;
  agg: Agg;
  window_secs: number;
  enabled: boolean;
  created_at: string;
  firing: boolean;
}

export interface NewAlertRule {
  name: string;
  metric: string;
  service?: string;
  comparator: Comparator;
  threshold: number;
  agg?: Agg;
  window_secs?: number;
}

export interface AlertEvent {
  id: number;
  rule_id: number;
  rule_name: string;
  metric: string;
  value: number | null;
  fired_at: string;
  resolved_at: string | null;
}

export const listAlerts = () => get<AlertRule[]>(`/api/alerts`);
export const createAlert = (r: NewAlertRule) => send<number>("POST", `/api/alerts`, r);
export const deleteAlert = (id: number) => send<null>("DELETE", `/api/alerts/${id}`);
export const listAlertEvents = (limit = 100) =>
  get<AlertEvent[]>(`/api/alerts/events?${qs({ limit })}`);
