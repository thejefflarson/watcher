import { useEffect, useState } from "react";
import { listMetrics, type MetricSummary } from "../api";
import { formatValue } from "../format";
import { useControls, rangeParams } from "../timerange";

// Inline sparkline — a small multiple, drawn with no axes or chrome.
function Sparkline({ values }: { values: number[] }) {
  const w = 90;
  const h = 18;
  if (values.length < 2) return <span className="muted">—</span>;
  const lo = Math.min(...values);
  const hi = Math.max(...values);
  const span = hi - lo || 1;
  // API returns newest-first; draw oldest→newest left to right.
  const pts = values
    .slice()
    .reverse()
    .map((v, i) => {
      const x = (i / (values.length - 1)) * (w - 2) + 1;
      const y = h - 1 - ((v - lo) / span) * (h - 2);
      return `${x.toFixed(1)},${y.toFixed(1)}`;
    })
    .join(" ");
  return (
    <svg className="spark" width={w} height={h} aria-hidden="true">
      <polyline points={pts} fill="none" stroke="#444" strokeWidth="1" />
    </svg>
  );
}

export default function MetricList({
  onSelect,
}: {
  onSelect: (m: MetricSummary) => void;
}) {
  const [metrics, setMetrics] = useState<MetricSummary[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [loaded, setLoaded] = useState(false);
  const [service, setService] = useState("");
  const { rangeKey, tick } = useControls();

  useEffect(() => {
    let active = true;
    const handle = setTimeout(() => {
      listMetrics({ service: service || undefined, ...rangeParams(rangeKey) })
        .then((m) => {
          if (active) {
            setMetrics(m);
            setError(null);
          }
        })
        .catch((e: unknown) => active && setError(String(e)))
        .finally(() => active && setLoaded(true));
    }, 250);
    return () => {
      active = false;
      clearTimeout(handle);
    };
  }, [service, rangeKey, tick]);

  if (error) return <p className="error">Failed to load: {error}</p>;

  return (
    <div>
      <div className="filters">
        <input
          placeholder="service"
          value={service}
          onChange={(e) => setService(e.target.value)}
        />
      </div>
      {!loaded ? (
        <p className="muted">Loading…</p>
      ) : metrics.length === 0 ? (
        <p className="muted">No metrics yet.</p>
      ) : (
        <table>
          <thead>
            <tr>
              <th>metric</th>
              <th>service</th>
              <th>kind</th>
              <th>recent</th>
              <th className="num">last</th>
              <th className="num">points</th>
            </tr>
          </thead>
          <tbody>
            {metrics.map((m) => (
              <tr
                key={m.name}
                className="clickable"
                onClick={() => onSelect(m)}
                title="View time series"
              >
                <td className="mono">{m.name}</td>
                <td>{m.service ?? "—"}</td>
                <td className="muted">{m.kind ?? "—"}</td>
                <td>{m.spark ? <Sparkline values={m.spark} /> : <span className="muted">—</span>}</td>
                <td className="num">{formatValue(m.last_value, m.unit)}</td>
                <td className="num muted">{m.points}</td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </div>
  );
}
