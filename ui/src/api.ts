// Same-origin in production (the server serves the UI); localhost in dev.
// Auth lives at the edge (Cloudflare Access), so the client sends no token.
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

export interface MetricSummary {
  name: string;
  service: string | null;
  kind: string | null;
  unit: string | null;
  last_time: string;
  last_value: number | null;
  spark: number[] | null;
  count_spark: number[] | null;
  dist: number[] | null;
  series_count: number | null;
}

export interface ServiceMapData {
  nodes: string[];
  edges: { source: string; target: string; calls: number }[];
}

async function get<T>(path: string): Promise<T> {
  const res = await fetch(`${BASE}${path}`);
  if (!res.ok) throw new Error(`${res.status} ${res.statusText}`);
  return (await res.json()) as T;
}

function qs(params: Record<string, string | number | boolean | undefined>): string {
  const q = new URLSearchParams();
  for (const [k, v] of Object.entries(params)) {
    if (v !== undefined && v !== "") q.set(k, String(v));
  }
  return q.toString();
}

export const listTraces = (
  p: {
    service?: string;
    name?: string;
    attr?: string;
    errors_only?: boolean;
    min_duration_ms?: number;
    limit?: number;
    from?: string;
    to?: string;
  } = {},
) => get<TraceSummary[]>(`/api/traces?${qs(p)}`);

export const getTrace = (traceId: string) => get<SpanRow[]>(`/api/traces/${traceId}`);

export const listLogs = (
  p: {
    service?: string;
    q?: string;
    trace_id?: string;
    span_id?: string;
    limit?: number;
    from?: string;
    to?: string;
    attr?: string;
  } = {},
) => get<LogRow[]>(`/api/logs?${qs(p)}`);

export const listMetrics = (
  p: { service?: string; limit?: number; from?: string; to?: string } = {},
) => get<MetricSummary[]>(`/api/metrics?${qs(p)}`);

export interface SeriesPoint {
  t: string;
  v: number | null;
}

export const getMetricSeries = (p: { name: string; service?: string; hours?: number }) =>
  get<SeriesPoint[]>(`/api/metrics/series?${qs(p)}`);

export interface LabeledPoint {
  label: string | null;
  t: string;
  v: number | null;
}

// Attribute keys a metric can be grouped by (e.g. k8s.pod.name / node / container).
export const getMetricDims = (name: string) => get<string[]>(`/api/metrics/dims?${qs({ name })}`);

// One labeled point stream — group client-side by `label` into per-dimension series.
export const getMetricSeriesGrouped = (p: { name: string; group_by: string; hours?: number }) =>
  get<LabeledPoint[]>(`/api/metrics/series_grouped?${qs(p)}`);

// Faceted series — one line per full attribute set (each pod × cpu, …). Gauges
// carry bucket values; monotonic sums (counters) carry per-second rates.
export interface FacetSeries {
  attrs: Record<string, string>;
  points: SeriesPoint[];
}
export interface FacetResponse {
  kind: string | null;
  rated: boolean;
  unit: string | null;
  series: FacetSeries[];
  truncated: number;
}
export const getMetricFacet = (p: { name: string; hours?: number }) =>
  get<FacetResponse>(`/api/metrics/facet?${qs(p)}`);

// Histogram distribution over time: a heatmap row (counts per value bucket) plus
// interpolated percentiles per time bucket.
export interface HistBucket {
  t: string;
  counts: number[];
  p50: number | null;
  p95: number | null;
  p99: number | null;
}
export interface HistResponse {
  bounds: number[];
  unit: string | null;
  buckets: HistBucket[];
}
export const getMetricHistogram = (p: { name: string; hours?: number }) =>
  get<HistResponse>(`/api/metrics/histogram?${qs(p)}`);

// Per-series histogram percentiles over time (one series per attribute set) —
// for expanding a histogram metric in the list into its series' p50/p95/p99.
export interface HistFacetPoint {
  t: string;
  p50: number | null;
  p95: number | null;
  p99: number | null;
}
export interface HistFacetSeries {
  attrs: Record<string, string>;
  points: HistFacetPoint[];
  dist: number[];
}
export interface HistFacetResponse {
  unit: string | null;
  bounds: number[];
  series: HistFacetSeries[];
  truncated: number;
}
export const getMetricHistFacet = (p: { name: string; hours?: number }) =>
  get<HistFacetResponse>(`/api/metrics/hist_facet?${qs(p)}`);

export const getServiceMap = () => get<ServiceMapData>(`/api/servicemap`);

export interface ServiceRed {
  service: string;
  spans: number;
  errors: number;
  error_rate: number;
  p50_ms: number | null;
  p95_ms: number | null;
  p99_ms: number | null;
}

export const listServices = (p: { from?: string; to?: string } = {}) =>
  get<ServiceRed[]>(`/api/services?${qs(p)}`);

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

export interface AlertEvent {
  id: number;
  rule_id: number;
  rule_name: string;
  metric: string;
  value: number | null;
  fired_at: string;
  resolved_at: string | null;
}

// Rules are declarative (managed in the chart values, reconciled on deploy), so
// the UI only reads them — no create/delete.
export const listAlerts = () => get<AlertRule[]>(`/api/alerts`);
export const listAlertEvents = (limit = 100) =>
  get<AlertEvent[]>(`/api/alerts/events?${qs({ limit })}`);
