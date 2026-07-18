import { useEffect, useState } from "react";
import { Link } from "react-router-dom";
import { listServices, type ServiceRed } from "../api";
import { useControls, rangeParams } from "../timerange";
import { useSort } from "../sort";
import { firstRunHint } from "../empty";
import SortHeader from "./SortHeader";
import { fmtDuration } from "./TraceList";

// RED table — one row per service: throughput, error rate, latency percentiles.
export default function Services() {
  const [rows, setRows] = useState<ServiceRed[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [loaded, setLoaded] = useState(false);
  const { rangeKey, tick } = useControls();

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

  const sort = useSort(rows, "spans");
  const { sorted } = sort;

  if (error) return <p className="error">Failed to load: {error}</p>;
  if (!loaded) return <p className="muted">Loading…</p>;
  if (rows.length === 0)
    return <p className="muted">{firstRunHint("traces", "services")}</p>;

  return (
    <table>
      <thead>
        <tr>
          <SortHeader sort={sort} field="service" label="Service" />
          <SortHeader sort={sort} field="spans" label="Spans" num />
          <SortHeader sort={sort} field="errors" label="Errors" num />
          <SortHeader sort={sort} field="error_rate" label="Error %" num />
          <SortHeader sort={sort} field="p50_ms" label="p50" num />
          <SortHeader sort={sort} field="p95_ms" label="p95" num />
          <SortHeader sort={sort} field="p99_ms" label="p99" num />
        </tr>
      </thead>
      <tbody>
        {sorted.map((r) => {
          const pct = (r.error_rate * 100).toFixed(r.error_rate >= 0.1 ? 0 : 1);
          return (
            <tr key={r.service} className="clickable">
              <td>
                <Link to={`/traces?service=${encodeURIComponent(r.service)}`} title="View traces">
                  {r.service}
                </Link>
              </td>
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
