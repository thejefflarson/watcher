import { useEffect, useState } from "react";
import { useSearchParams } from "react-router-dom";
import { listTraces, listServices, type TraceSummary } from "../api";
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
  // Service lives in the URL so service-map drill-downs (?service=X) land here.
  const service = params.get("service") ?? "";
  const setService = (v: string) => {
    const next = new URLSearchParams(params);
    if (v) next.set("service", v);
    else next.delete("service");
    setParams(next, { replace: true });
  };
  const [name, setName] = useState("");
  const [attr, setAttr] = useState("");
  const [errorsOnly, setErrorsOnly] = useState(false);
  const [minDuration, setMinDuration] = useState("");
  const [services, setServices] = useState<string[]>([]);

  // Populate the service dropdown from the services that have traces in range.
  useEffect(() => {
    let active = true;
    listServices(rangeParams(rangeKey))
      .then((rows) => active && setServices(rows.map((r) => r.service).sort()))
      .catch(() => active && setServices([]));
    return () => {
      active = false;
    };
  }, [rangeKey, tick]);

  useEffect(() => {
    let active = true;
    const handle = setTimeout(() => {
      const min = Number(minDuration);
      listTraces({
        limit: 100,
        service: service || undefined,
        name: name || undefined,
        attr: attr.includes("=") ? attr : undefined,
        errors_only: errorsOnly || undefined,
        min_duration_ms: minDuration && !Number.isNaN(min) ? min : undefined,
        ...rangeParams(rangeKey),
      })
        .then((t) => {
          if (active) {
            setTraces(t);
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
  }, [service, name, attr, errorsOnly, minDuration, rangeKey, tick]);

  const { sorted, onSort, indicator } = useSort(traces, "start_time");

  const filters = (
    <div className="filters">
      <select value={service} onChange={(e) => setService(e.target.value)}>
        <option value="">all services</option>
        {/* keep a drill-down service selectable even if it's outside the window's list */}
        {(services.includes(service) || !service ? services : [service, ...services]).map(
          (s) => (
            <option key={s} value={s}>
              {s}
            </option>
          ),
        )}
      </select>
      <input
        placeholder="root span name…"
        value={name}
        onChange={(e) => setName(e.target.value)}
      />
      <input
        placeholder="attribute key=value"
        value={attr}
        onChange={(e) => setAttr(e.target.value)}
        title="Filter to traces with a span attribute, e.g. http.method=GET"
      />
      <input
        type="number"
        min={0}
        placeholder="min ms"
        value={minDuration}
        onChange={(e) => setMinDuration(e.target.value)}
        title="Only traces at least this many milliseconds long"
      />
      <label className="checkbox" title="Only traces containing an error span">
        <input
          type="checkbox"
          checked={errorsOnly}
          onChange={(e) => setErrorsOnly(e.target.checked)}
        />
        errors only
      </label>
    </div>
  );

  return (
    <>
      {filters}
      {error && <p className="error">Failed to load: {error}</p>}
      {!loaded && !error && <p className="muted">Loading traces…</p>}
      {loaded && traces.length === 0 && !error && (
        <p className="muted">No traces match.</p>
      )}
      {traces.length > 0 && (
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
      )}
    </>
  );
}
