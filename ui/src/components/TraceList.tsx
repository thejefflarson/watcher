import { useEffect, useState } from "react";
import { listTraces, type TraceSummary } from "../api";
import { useControls, rangeParams } from "../timerange";

export function fmtDuration(ms: number): string {
  if (ms < 1) return `${(ms * 1000).toFixed(0)}µs`;
  if (ms < 1000) return `${ms.toFixed(1)}ms`;
  return `${(ms / 1000).toFixed(2)}s`;
}

export default function TraceList({ onSelect }: { onSelect: (traceId: string) => void }) {
  const [traces, setTraces] = useState<TraceSummary[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const { rangeKey, tick } = useControls();

  useEffect(() => {
    let active = true;
    setLoading(true);
    listTraces({ limit: 100, ...rangeParams(rangeKey) })
      .then((t) => {
        if (active) {
          setTraces(t);
          setError(null);
        }
      })
      .catch((e: unknown) => active && setError(String(e)))
      .finally(() => active && setLoading(false));
    return () => {
      active = false;
    };
  }, [rangeKey, tick]);

  if (loading) return <p className="muted">Loading traces…</p>;
  if (error) return <p className="error">Failed to load: {error}</p>;
  if (traces.length === 0)
    return <p className="muted">No traces yet. Point an OTLP exporter at :4318.</p>;

  return (
    <table>
      <thead>
        <tr>
          <th>Service</th>
          <th>Root span</th>
          <th>Started</th>
          <th className="num">Duration</th>
          <th className="num">Spans</th>
          <th className="num">Errors</th>
        </tr>
      </thead>
      <tbody>
        {traces.map((t) => (
          <tr key={t.trace_id} className="clickable" onClick={() => onSelect(t.trace_id)}>
            <td>{t.service ?? "—"}</td>
            <td>{t.root_name ?? "—"}</td>
            <td>{new Date(t.start_time).toLocaleString()}</td>
            <td className="num">{fmtDuration(t.duration_ms)}</td>
            <td className="num">{t.span_count}</td>
            <td className={"num" + (t.error_count > 0 ? " err" : "")}>{t.error_count}</td>
          </tr>
        ))}
      </tbody>
    </table>
  );
}
