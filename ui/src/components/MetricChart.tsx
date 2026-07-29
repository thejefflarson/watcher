import { useEffect, useMemo, useState } from "react";
import { Link, useNavigate, useSearchParams } from "react-router";
import {
  getMetricExemplars,
  getMetricFacet,
  getMetricHistogram,
  type ExemplarPoint,
  type HistResponse,
  type SeriesPoint,
} from "../api";
import { formatValue } from "../format";
import { facetLabels } from "../metricLabels";
import { traceHref } from "../links";

const RANGES: { label: string; hours: number }[] = [
  { label: "1h", hours: 1 },
  { label: "6h", hours: 6 },
  { label: "24h", hours: 24 },
  { label: "7d", hours: 24 * 7 },
];

const COLORS = [
  "#333333",
  "#5b9dff",
  "#a00000",
  "#2a8a4a",
  "#8a5a00",
  "#7a3aa0",
  "#0a8a8a",
  "#b05a2a",
];

interface Series {
  label: string;
  points: SeriesPoint[];
}

// One raw point that carries a sampled trace exemplar, positioned by
// time + value so `Chart` can plot it as a marker on the line it belongs to.
interface ExemplarMark {
  t: string;
  v: number | null;
  traceId: string;
}

// A small multi-line chart drawn by hand — hairline axes, ink for the lines.
// `threshold` (an alert rule's bound) is drawn as a dashed reference line and
// `firingFrom` (ms) shades the still-open firing window when deep-linked here.
// `exemplars` overlays a marker per point with a sampled trace (correlational —
// "a trace recorded here" — never causal); clicking one calls `onExemplarClick`.
function Chart({
  series,
  unit,
  threshold,
  firingFrom,
  desc,
  exemplars,
  onExemplarClick,
}: {
  series: Series[];
  unit?: string | null;
  threshold?: number | null;
  firingFrom?: number | null;
  desc: string;
  exemplars?: ExemplarMark[];
  onExemplarClick?: (traceId: string) => void;
}) {
  const w = 720;
  const h = 240;
  const pad = { l: 56, r: 12, t: 12, b: 24 };

  const geom = useMemo(() => {
    const all = series.flatMap((s) => s.points).filter((p) => p.v !== null);
    if (all.length < 2) return null;
    const vals = all.map((p) => p.v as number);
    const hasThreshold = threshold != null && Number.isFinite(threshold);
    // Fold the threshold into the value range so the reference line is on-canvas.
    const lo = Math.min(...vals, 0, hasThreshold ? threshold : Infinity);
    const hi = Math.max(...vals, hasThreshold ? threshold : -Infinity);
    const span = hi - lo || 1;
    const times = all.map((p) => new Date(p.t).getTime());
    const t0 = Math.min(...times);
    const t1 = Math.max(...times);
    const tSpan = t1 - t0 || 1;
    const x = (t: number) => pad.l + ((t - t0) / tSpan) * (w - pad.l - pad.r);
    const y = (v: number) => h - pad.b - ((v - lo) / span) * (h - pad.t - pad.b);
    const lines = series.map((s) => ({
      label: s.label,
      d: s.points
        .filter((p) => p.v !== null)
        .map((p) => `${x(new Date(p.t).getTime()).toFixed(1)},${y(p.v as number).toFixed(1)}`)
        .join(" "),
    }));
    const thresholdY = hasThreshold ? y(threshold) : null;
    // Clamp the firing-window start to the plotted range (it may predate it).
    const firingX =
      firingFrom != null ? Math.max(pad.l, Math.min(x(firingFrom), w - pad.r)) : null;
    return { lo, hi, t0, t1, lines, thresholdY, firingX, x, y };
  }, [series, threshold, firingFrom]);

  if (!geom) return <p className="muted">Not enough data to plot.</p>;

  const fmtTime = (ms: number) =>
    new Date(ms).toLocaleString(undefined, {
      month: "short",
      day: "numeric",
      hour: "2-digit",
      minute: "2-digit",
    });

  // A one-line summary of what the chart shows, for screen readers — the shape
  // is decorative, the range is the datum: "payments p95 latency, 6h, 12–340 ms".
  const ariaLabel = `${desc}, ${formatValue(geom.lo, unit)}–${formatValue(geom.hi, unit)}`;

  return (
    <div>
      <svg className="chart" width={w} height={h} role="img" aria-label={ariaLabel}>
        <line x1={pad.l} y1={pad.t} x2={pad.l} y2={h - pad.b} stroke="var(--rule)" />
        <line x1={pad.l} y1={h - pad.b} x2={w - pad.r} y2={h - pad.b} stroke="var(--rule)" />
        <text x={pad.l - 6} y={pad.t + 4} textAnchor="end" className="tick">
          {formatValue(geom.hi, unit)}
        </text>
        <text x={pad.l - 6} y={h - pad.b} textAnchor="end" className="tick">
          {formatValue(geom.lo, unit)}
        </text>
        <text x={pad.l} y={h - 6} textAnchor="start" className="tick">
          {fmtTime(geom.t0)}
        </text>
        <text x={w - pad.r} y={h - 6} textAnchor="end" className="tick">
          {fmtTime(geom.t1)}
        </text>
        {geom.firingX != null && (
          <rect
            x={geom.firingX}
            y={pad.t}
            width={Math.max(0, w - pad.r - geom.firingX)}
            height={h - pad.t - pad.b}
            fill="var(--error)"
            opacity={0.07}
          />
        )}
        {geom.thresholdY != null && threshold != null && (
          <g>
            <line
              x1={pad.l}
              y1={geom.thresholdY}
              x2={w - pad.r}
              y2={geom.thresholdY}
              stroke="var(--error)"
              strokeWidth="1"
              strokeDasharray="4 3"
            />
            <text
              x={w - pad.r}
              y={geom.thresholdY - 3}
              textAnchor="end"
              className="tick"
              fill="var(--error)"
            >
              threshold {formatValue(threshold, unit)}
            </text>
          </g>
        )}
        {geom.lines.map((l, i) => (
          <polyline
            key={l.label}
            points={l.d}
            fill="none"
            stroke={COLORS[i % COLORS.length]}
            strokeWidth="1"
          />
        ))}
        {exemplars?.map((e) => {
          if (e.v == null) return null;
          const t = new Date(e.t).getTime();
          if (t < geom.t0 || t > geom.t1) return null;
          return (
            <circle
              key={`${e.traceId}-${e.t}`}
              className="exemplar-dot"
              cx={geom.x(t)}
              cy={geom.y(e.v)}
              r={3.5}
              role="link"
              tabIndex={0}
              aria-label={`View trace ${e.traceId} recorded near this point`}
              onClick={() => onExemplarClick?.(e.traceId)}
              onKeyDown={(ev) => {
                if (ev.key === "Enter" || ev.key === " ") {
                  ev.preventDefault();
                  onExemplarClick?.(e.traceId);
                }
              }}
            >
              <title>{`trace ${e.traceId} — open`}</title>
            </circle>
          );
        })}
      </svg>
      {series.length > 1 && (
        <div className="legend">
          {series.map((s, i) => (
            <span key={s.label} className="legend-item">
              <span className="swatch" style={{ background: COLORS[i % COLORS.length] }} />
              <span className="mono">{s.label}</span>
            </span>
          ))}
        </div>
      )}
    </div>
  );
}

// Heatmap of a histogram over time: x = time bucket, y = value bucket, ink
// density = observation count (log-scaled so a few outliers stay visible).
function Heatmap({ data, desc }: { data: HistResponse; desc: string }) {
  const { bounds, buckets, unit } = data;
  const rows = bounds.length + 1; // +Inf overflow row on top
  const w = 720;
  const cellH = 16;
  const pad = { l: 56, r: 12, t: 8, b: 22 };
  const h = pad.t + pad.b + rows * cellH;

  const maxCount = useMemo(
    () => Math.max(1, ...buckets.flatMap((b) => b.counts)),
    [buckets],
  );
  if (buckets.length === 0) return <p className="muted">No histogram data.</p>;

  const cols = buckets.length;
  const cellW = (w - pad.l - pad.r) / cols;
  const norm = (c: number) => (c <= 0 ? 0 : Math.log(c + 1) / Math.log(maxCount + 1));
  const rowLabel = (r: number) =>
    r === bounds.length ? "∞" : formatValue(bounds[r], unit);
  const fmtTime = (s: string) =>
    new Date(s).toLocaleTimeString(undefined, { hour: "2-digit", minute: "2-digit" });

  return (
    <svg className="chart" width={w} height={h} role="img" aria-label={`${desc} density heatmap`}>
      {/* y ticks: a few bucket bounds */}
      {Array.from({ length: rows }).map((_, r) => {
        const y = pad.t + (rows - 1 - r) * cellH;
        const show = rows <= 8 || r % Math.ceil(rows / 8) === 0 || r === rows - 1;
        return show ? (
          <text key={`y${r}`} x={pad.l - 6} y={y + cellH - 4} textAnchor="end" className="tick">
            {rowLabel(r)}
          </text>
        ) : null;
      })}
      {buckets.map((b, ci) =>
        b.counts.map((c, r) => {
          const o = norm(c);
          if (o <= 0) return null;
          return (
            <rect
              key={`${ci}-${r}`}
              x={pad.l + ci * cellW}
              y={pad.t + (rows - 1 - r) * cellH}
              width={Math.max(1, cellW - 0.5)}
              height={cellH - 0.5}
              fill="#1a3a6a"
              opacity={0.12 + 0.88 * o}
            >
              <title>{`${rowLabel(r)} · ${c}`}</title>
            </rect>
          );
        }),
      )}
      <text x={pad.l} y={h - 6} textAnchor="start" className="tick">
        {fmtTime(buckets[0].t)}
      </text>
      <text x={w - pad.r} y={h - 6} textAnchor="end" className="tick">
        {fmtTime(buckets[cols - 1].t)}
      </text>
    </svg>
  );
}

const MAX_LINES = 12;

// Gauge/sum: one line per series. Sums (counters) arrive as per-second rates.
function FacetView({
  name,
  service,
  hours,
  unit,
  threshold,
  firingFrom,
  rangeLabel,
}: {
  name: string;
  service: string | null;
  hours: number;
  unit?: string | null;
  threshold?: number | null;
  firingFrom?: number | null;
  rangeLabel: string;
}) {
  const navigate = useNavigate();
  const [data, setData] = useState<{
    series: Series[];
    rated: boolean;
    truncated: number;
  } | null>(null);
  const [error, setError] = useState<string | null>(null);
  // Best-effort — exemplars only exist in the raw-metric window, so an empty
  // result is the normal case, not a failure.
  const [exemplars, setExemplars] = useState<ExemplarPoint[]>([]);

  useEffect(() => {
    let active = true;
    getMetricFacet({ name, hours })
      .then((f) => {
        if (!active) return;
        const labels = facetLabels(f.series);
        const series = f.series.map((s, i) => ({ label: labels[i], points: s.points }));
        // Cap drawn lines; the API already capped series and reports the rest.
        setData({
          series: series.slice(0, MAX_LINES),
          rated: f.rated,
          truncated: f.truncated + Math.max(0, series.length - MAX_LINES),
        });
        setError(null);
      })
      .catch((e: unknown) => active && setError(String(e)));
    return () => {
      active = false;
    };
  }, [name, hours]);

  useEffect(() => {
    let active = true;
    getMetricExemplars({ name, service: service ?? undefined, hours })
      .then((e) => active && setExemplars(e))
      .catch(() => active && setExemplars([]));
    return () => {
      active = false;
    };
  }, [name, service, hours]);

  if (error) return <p className="error">Failed to load: {error}</p>;
  if (!data) return <p className="muted">Loading…</p>;
  return (
    <div>
      <p className="muted small">
        {data.rated
          ? "counter — per-second rate"
          : "gauge — value"}
        {data.series.length > 1 ? ` · ${data.series.length} series` : ""}
        {data.truncated > 0 ? ` · +${data.truncated} more not shown` : ""}
        {exemplars.length > 0
          ? ` · ${exemplars.length} traces recorded here (○, click to open)`
          : ""}
      </p>
      <Chart
        series={data.series}
        unit={data.rated ? `${unit ?? ""}/s` : unit}
        threshold={threshold}
        firingFrom={firingFrom}
        desc={`${name} ${data.rated ? "per-second rate" : "value"}, ${rangeLabel}`}
        exemplars={exemplars.map((e) => ({ t: e.t, v: e.v, traceId: e.trace_id }))}
        onExemplarClick={(traceId) => navigate(traceHref(traceId, { service }))}
      />
    </div>
  );
}

// Histogram: heatmap of the distribution plus interpolated p50/p95/p99 lines.
function HistogramView({
  name,
  hours,
  threshold,
  firingFrom,
  rangeLabel,
}: {
  name: string;
  hours: number;
  threshold?: number | null;
  firingFrom?: number | null;
  rangeLabel: string;
}) {
  const [data, setData] = useState<HistResponse | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let active = true;
    getMetricHistogram({ name, hours })
      .then((h) => active && (setData(h), setError(null)))
      .catch((e: unknown) => active && setError(String(e)));
    return () => {
      active = false;
    };
  }, [name, hours]);

  const pctSeries: Series[] = useMemo(() => {
    if (!data) return [];
    const pick = (k: "p50" | "p95" | "p99"): Series => ({
      label: k,
      points: data.buckets.map((b) => ({ t: b.t, v: b[k] })),
    });
    return [pick("p99"), pick("p95"), pick("p50")];
  }, [data]);

  if (error) return <p className="error">Failed to load: {error}</p>;
  if (!data) return <p className="muted">Loading…</p>;
  if (data.buckets.length === 0)
    return <p className="muted">No histogram data in this range.</p>;
  return (
    <div>
      <p className="muted small">histogram — distribution &amp; percentiles</p>
      <Chart
        series={pctSeries}
        unit={data.unit}
        threshold={threshold}
        firingFrom={firingFrom}
        desc={`${name} percentiles (p50/p95/p99), ${rangeLabel}`}
      />
      <p className="muted small heatmap-label">density heatmap</p>
      <Heatmap data={data} desc={`${name}, ${rangeLabel}`} />
    </div>
  );
}

export default function MetricChart({
  name,
  service,
  unit,
  kind,
  onBack,
}: {
  name: string;
  service: string | null;
  unit?: string | null;
  kind?: string | null;
  onBack: () => void;
}) {
  const [hours, setHours] = useState(6);
  // Alert deep-link overlay — read straight from the URL so the /metrics route in
  // App.tsx stays unchanged. Absent for ordinary metric navigation.
  const [params] = useSearchParams();
  const thresholdRaw = params.get("threshold");
  const threshold =
    thresholdRaw && Number.isFinite(Number(thresholdRaw)) ? Number(thresholdRaw) : null;
  const firingRaw = params.get("firing_from");
  const firingFrom =
    firingRaw && !Number.isNaN(Date.parse(firingRaw)) ? Date.parse(firingRaw) : null;
  const rangeLabel = RANGES.find((r) => r.hours === hours)?.label ?? `${hours}h`;

  // One-action deep link to the trace list scoped to this chart's service +
  // displayed window — the window is the picker's hours-back-from-now
  // range, matching what's actually plotted. Correlational ("traces in this
  // window"), not a claim that any one of them caused what the chart shows.
  const tracesHref = useMemo(() => {
    const now = Date.now();
    const qs = new URLSearchParams({
      from: new Date(now - hours * 60 * 60 * 1000).toISOString(),
      to: new Date(now).toISOString(),
    });
    if (service) qs.set("service", service);
    return `/traces?${qs.toString()}`;
  }, [service, hours]);

  return (
    <div className="metric-chart">
      <button className="back" onClick={onBack}>
        ← metrics
      </button>
      <h2>
        <span className="mono">{name}</span>
        {service ? <span className="muted"> · {service}</span> : null}
      </h2>
      <div className="filters">
        {RANGES.map((r) => (
          <button
            key={r.hours}
            className={hours === r.hours ? "range active" : "range"}
            onClick={() => setHours(r.hours)}
          >
            {r.label}
          </button>
        ))}
        <Link className="back traces-link" to={tracesHref}>
          traces in this window →
        </Link>
      </div>
      {kind === "histogram" ? (
        <HistogramView
          name={name}
          hours={hours}
          threshold={threshold}
          firingFrom={firingFrom}
          rangeLabel={rangeLabel}
        />
      ) : (
        <FacetView
          name={name}
          service={service}
          hours={hours}
          unit={unit}
          threshold={threshold}
          firingFrom={firingFrom}
          rangeLabel={rangeLabel}
        />
      )}
    </div>
  );
}
