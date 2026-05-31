import { useEffect, useState } from "react";
import { useSearchParams } from "react-router-dom";
import { listTraces, type TraceSummary } from "../api";
import { useControls, rangeParams } from "../timerange";
import { useSort } from "../sort";

export function fmtDuration(ms: number): string {
  if (ms < 1) return `${(ms * 1000).toFixed(0)}µs`;
  if (ms < 1000) return `${ms.toFixed(1)}ms`;
  return `${(ms / 1000).toFixed(2)}s`;
}

export default function TraceList({ onSelect }: { onSelect: (traceId: string) => void }) {
  const [traces, setTraces] = useState<TraceSummary[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [loaded, setLoaded] = useState(false);
  const { rangeKey, tick } = useControls();
  const [params, setParams] = useSearchParams();
  const serviceFilter = params.get("service");

  useEffect(() => {
    let active = true;
    listTraces({ limit: 100, service: serviceFilter ?? undefined, ...rangeParams(rangeKey) })
      .then((t) => {
        if (active) {
          setTraces(t);
          setError(null);
        }
      })
      .catch((e: unknown) => active && setError(String(e)))
      .finally(() => active && setLoaded(true));
    return () => {
      active = false;
    };
  }, [serviceFilter, rangeKey, tick]);

  const { sorted, onSort, indicator } = useSort(traces, "start_time");

  if (!loaded) return <p className="muted">Loading traces…</p>;
  if (error) return <p className="error">Failed to load: {error}</p>;

  const banner = serviceFilter && (
    <p className="filter-banner">
      traces for <strong>{serviceFilter}</strong>{" "}
      <button className="xlink" onClick={() => setParams({})}>
        clear
      </button>
    </p>
  );

  if (traces.length === 0)
    return (
      <>
        {banner}
        <p className="muted">No traces in this window.</p>
      </>
    );

  return (
    <>
      {banner}
      <table>
      <thead>
        <tr>
          <th className="sortable" onClick={() => onSort("service")}>Service{indicator("service")}</th>
          <th className="sortable" onClick={() => onSort("root_name")}>Root span{indicator("root_name")}</th>
          <th className="sortable" onClick={() => onSort("start_time")}>Started{indicator("start_time")}</th>
          <th className="num sortable" onClick={() => onSort("duration_ms")}>Duration{indicator("duration_ms")}</th>
          <th className="num sortable" onClick={() => onSort("span_count")}>Spans{indicator("span_count")}</th>
          <th className="num sortable" onClick={() => onSort("error_count")}>Errors{indicator("error_count")}</th>
        </tr>
      </thead>
      <tbody>
        {sorted.map((t) => (
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
    </>
  );
}
