import { useEffect, useState } from "react";
import { useNavigate } from "react-router-dom";
import { listServices, type ServiceRed } from "../api";
import { useControls, rangeParams } from "../timerange";
import { useSort } from "../sort";
import { fmtDuration } from "./TraceList";

// RED table — one row per service: throughput, error rate, latency percentiles.
export default function Services() {
  const [rows, setRows] = useState<ServiceRed[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [loaded, setLoaded] = useState(false);
  const { rangeKey, tick } = useControls();
  const navigate = useNavigate();

  useEffect(() => {
    let active = true;
    listServices(rangeParams(rangeKey))
      .then((r) => active && (setRows(r), setError(null)))
      .catch((e: unknown) => active && setError(String(e)))
      .finally(() => active && setLoaded(true));
    return () => {
      active = false;
    };
  }, [rangeKey, tick]);

  const { sorted, onSort, indicator } = useSort(rows, "spans");

  if (error) return <p className="error">Failed to load: {error}</p>;
  if (!loaded) return <p className="muted">Loading…</p>;
  if (rows.length === 0) return <p className="muted">No spans in this window.</p>;

  return (
    <table>
      <thead>
        <tr>
          <th className="sortable" onClick={() => onSort("service")}>Service{indicator("service")}</th>
          <th className="num sortable" onClick={() => onSort("spans")}>Spans{indicator("spans")}</th>
          <th className="num sortable" onClick={() => onSort("errors")}>Errors{indicator("errors")}</th>
          <th className="num sortable" onClick={() => onSort("error_rate")}>Error %{indicator("error_rate")}</th>
          <th className="num sortable" onClick={() => onSort("p50_ms")}>p50{indicator("p50_ms")}</th>
          <th className="num sortable" onClick={() => onSort("p95_ms")}>p95{indicator("p95_ms")}</th>
          <th className="num sortable" onClick={() => onSort("p99_ms")}>p99{indicator("p99_ms")}</th>
        </tr>
      </thead>
      <tbody>
        {sorted.map((r) => {
          const pct = (r.error_rate * 100).toFixed(r.error_rate >= 0.1 ? 0 : 1);
          return (
            <tr
              key={r.service}
              className="clickable"
              onClick={() => navigate(`/traces?service=${encodeURIComponent(r.service)}`)}
              title="View traces"
            >
              <td>{r.service}</td>
              <td className="num">{r.spans}</td>
              <td className={"num" + (r.errors > 0 ? " err" : "")}>{r.errors}</td>
              <td className={"num" + (r.error_rate > 0 ? " err" : "")}>{pct}%</td>
              <td className="num">{r.p50_ms === null ? "—" : fmtDuration(r.p50_ms)}</td>
              <td className="num">{r.p95_ms === null ? "—" : fmtDuration(r.p95_ms)}</td>
              <td className="num">{r.p99_ms === null ? "—" : fmtDuration(r.p99_ms)}</td>
            </tr>
          );
        })}
      </tbody>
    </table>
  );
}
