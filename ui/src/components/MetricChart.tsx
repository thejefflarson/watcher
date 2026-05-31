import { useEffect, useMemo, useState } from "react";
import { getMetricSeries, type SeriesPoint } from "../api";
import { formatValue } from "../format";

const RANGES: { label: string; hours: number }[] = [
  { label: "1h", hours: 1 },
  { label: "6h", hours: 6 },
  { label: "24h", hours: 24 },
  { label: "7d", hours: 24 * 7 },
];

// A small line chart drawn by hand — hairline axes, ink only for the line.
function Chart({ points, unit }: { points: SeriesPoint[]; unit?: string | null }) {
  const w = 720;
  const h = 240;
  const pad = { l: 48, r: 12, t: 12, b: 24 };

  const geom = useMemo(() => {
    const vals = points.map((p) => p.v).filter((v): v is number => v !== null);
    if (vals.length < 2) return null;
    const lo = Math.min(...vals);
    const hi = Math.max(...vals);
    const span = hi - lo || 1;
    const t0 = new Date(points[0].t).getTime();
    const t1 = new Date(points[points.length - 1].t).getTime();
    const tSpan = t1 - t0 || 1;
    const x = (t: number) => pad.l + ((t - t0) / tSpan) * (w - pad.l - pad.r);
    const y = (v: number) => h - pad.b - ((v - lo) / span) * (h - pad.t - pad.b);
    const line = points
      .filter((p) => p.v !== null)
      .map((p) => `${x(new Date(p.t).getTime()).toFixed(1)},${y(p.v as number).toFixed(1)}`)
      .join(" ");
    return { lo, hi, t0, t1, line, x, y };
  }, [points]);

  if (!geom) return <p className="muted">Not enough data to plot.</p>;

  const fmtTime = (ms: number) =>
    new Date(ms).toLocaleString(undefined, {
      month: "short",
      day: "numeric",
      hour: "2-digit",
      minute: "2-digit",
    });

  return (
    <svg className="chart" width={w} height={h} role="img">
      {/* y axis: min/max ticks only */}
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
      <polyline points={geom.line} fill="none" stroke="#333" strokeWidth="1" />
    </svg>
  );
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
  const [points, setPoints] = useState<SeriesPoint[]>([]);
  const [hours, setHours] = useState(24);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let active = true;
    getMetricSeries({ name, service: service ?? undefined, hours })
      .then((p) => active && (setPoints(p), setError(null)))
      .catch((e: unknown) => active && setError(String(e)));
    return () => {
      active = false;
    };
  }, [name, service, hours]);

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
      {error ? (
        <p className="error">Failed to load: {error}</p>
      ) : (
        <Chart points={points} unit={unit} />
      )}
    </div>
  );
}
