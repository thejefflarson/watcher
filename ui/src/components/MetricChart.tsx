import { useEffect, useMemo, useState } from "react";
import {
  getMetricDims,
  getMetricSeries,
  getMetricSeriesGrouped,
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
  const pad = { l: 48, r: 12, t: 12, b: 24 };

  const geom = useMemo(() => {
    const all = series.flatMap((s) => s.points).filter((p) => p.v !== null);
    if (all.length < 2) return null;
    const vals = all.map((p) => p.v as number);
    const lo = Math.min(...vals);
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

const MAX_SERIES = 12;

function groupLabeled(
  rows: { label: string | null; t: string; v: number | null }[],
): Series[] {
  const map = new Map<string, SeriesPoint[]>();
  for (const r of rows) {
    const k = r.label ?? "—";
    let arr = map.get(k);
    if (!arr) {
      arr = [];
      map.set(k, arr);
    }
    arr.push({ t: r.t, v: r.v });
  }
  return [...map.entries()]
    .sort((a, b) => b[1].length - a[1].length)
    .slice(0, MAX_SERIES)
    .map(([label, points]) => ({ label, points }));
}

export default function MetricChart({
  name,
  service,
  unit,
  onBack,
}: {
  name: string;
  service: string | null;
  unit?: string | null;
  onBack: () => void;
}) {
  const [series, setSeries] = useState<Series[]>([]);
  const [hours, setHours] = useState(24);
  const [dims, setDims] = useState<string[]>([]);
  const [groupBy, setGroupBy] = useState("");
  const [error, setError] = useState<string | null>(null);

  // Available group-by dimensions for this metric.
  useEffect(() => {
    let active = true;
    getMetricDims(name)
      .then((d) => active && setDims(d))
      .catch(() => active && setDims([]));
    return () => {
      active = false;
    };
  }, [name]);

  // The series — single (aggregate) or grouped by a dimension.
  useEffect(() => {
    let active = true;
    const load = groupBy
      ? getMetricSeriesGrouped({ name, group_by: groupBy, hours }).then(groupLabeled)
      : getMetricSeries({ name, service: service ?? undefined, hours }).then((p) => [
          { label: "all", points: p },
        ]);
    load
      .then((s) => active && (setSeries(s), setError(null)))
      .catch((e: unknown) => active && setError(String(e)));
    return () => {
      active = false;
    };
  }, [name, service, hours, groupBy]);

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
        {dims.length > 0 && (
          <select value={groupBy} onChange={(e) => setGroupBy(e.target.value)}>
            <option value="">group by…</option>
            {dims.map((d) => (
              <option key={d} value={d}>
                {d}
              </option>
            ))}
          </select>
        )}
      </div>
      {error ? (
        <p className="error">Failed to load: {error}</p>
      ) : (
        <Chart series={series} unit={unit} />
      )}
    </div>
  );
}
