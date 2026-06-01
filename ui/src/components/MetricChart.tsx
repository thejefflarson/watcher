import { useEffect, useMemo, useState } from "react";
import {
  getMetricFacet,
  getMetricHistogram,
  type FacetSeries,
  type HistResponse,
  type SeriesPoint,
} from "../api";
import { formatValue } from "../format";

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

// A small multi-line chart drawn by hand — hairline axes, ink for the lines.
function Chart({ series, unit }: { series: Series[]; unit?: string | null }) {
  const w = 720;
  const h = 240;
  const pad = { l: 56, r: 12, t: 12, b: 24 };

  const geom = useMemo(() => {
    const all = series.flatMap((s) => s.points).filter((p) => p.v !== null);
    if (all.length < 2) return null;
    const vals = all.map((p) => p.v as number);
    const lo = Math.min(...vals, 0);
    const hi = Math.max(...vals);
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
    return { lo, hi, t0, t1, lines };
  }, [series]);

  if (!geom) return <p className="muted">Not enough data to plot.</p>;

  const fmtTime = (ms: number) =>
    new Date(ms).toLocaleString(undefined, {
      month: "short",
      day: "numeric",
      hour: "2-digit",
      minute: "2-digit",
    });

  return (
    <div>
      <svg className="chart" width={w} height={h} role="img">
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
        {geom.lines.map((l, i) => (
          <polyline
            key={l.label}
            points={l.d}
            fill="none"
            stroke={COLORS[i % COLORS.length]}
            strokeWidth="1"
          />
        ))}
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
function Heatmap({ data }: { data: HistResponse }) {
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
    <svg className="chart" width={w} height={h} role="img">
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

// Strip the noisy `k8s.` prefix so labels read pod.name=… not k8s.pod.name=….
const shortKey = (k: string) => k.replace(/^k8s\./, "");

// Label each facet series by only the attribute keys that vary across the set,
// dropping uid-style keys when a friendlier varying key is available.
function facetLabels(series: FacetSeries[]): string[] {
  if (series.length <= 1) return series.map(() => "all");
  const keys = new Set<string>();
  series.forEach((s) => Object.keys(s.attrs).forEach((k) => keys.add(k)));
  const varying = [...keys].filter((k) => {
    const vals = new Set(series.map((s) => s.attrs[k] ?? ""));
    return vals.size > 1;
  });
  const friendly = varying.filter((k) => !k.endsWith(".uid") && !k.endsWith("id"));
  const labelKeys = (friendly.length ? friendly : varying).slice(0, 3);
  return series.map(
    (s) =>
      labelKeys.map((k) => `${shortKey(k)}=${s.attrs[k] ?? "∅"}`).join(" · ") || "—",
  );
}

const MAX_LINES = 12;

// Gauge/sum: one line per series. Sums (counters) arrive as per-second rates.
function FacetView({ name, hours, unit }: { name: string; hours: number; unit?: string | null }) {
  const [data, setData] = useState<{
    series: Series[];
    rated: boolean;
    truncated: number;
  } | null>(null);
  const [error, setError] = useState<string | null>(null);

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
      </p>
      <Chart series={data.series} unit={data.rated ? `${unit ?? ""}/s` : unit} />
    </div>
  );
}

// Histogram: heatmap of the distribution plus interpolated p50/p95/p99 lines.
function HistogramView({ name, hours }: { name: string; hours: number }) {
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
      <Chart series={pctSeries} unit={data.unit} />
      <p className="muted small heatmap-label">density heatmap</p>
      <Heatmap data={data} />
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
      </div>
      {kind === "histogram" ? (
        <HistogramView name={name} hours={hours} />
      ) : (
        <FacetView name={name} hours={hours} unit={unit} />
      )}
    </div>
  );
}
