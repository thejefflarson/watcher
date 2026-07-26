import { useEffect, useState } from "react";
import { Link, useSearchParams } from "react-router-dom";
import { listTraces, type TraceSummary } from "../api";
import { useControls, rangeParams } from "../timerange";
import { useSort } from "../sort";
import { firstRunHint } from "../empty";
import SortHeader from "./SortHeader";

export function fmtDuration(ms: number): string {
  if (ms < 1) return `${(ms * 1000).toFixed(0)}µs`;
  if (ms < 1000) return `${ms.toFixed(1)}ms`;
  return `${(ms / 1000).toFixed(2)}s`;
}

export default function TraceList({ to }: { to: (traceId: string) => string }) {
  const [traces, setTraces] = useState<TraceSummary[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [loaded, setLoaded] = useState(false);
  // Service focus is global (header select, mirrored to `?service=`).
  const { rangeKey, service, tick } = useControls();
  const [name, setName] = useState("");
  const [attr, setAttr] = useState("");
  const [errorsOnly, setErrorsOnly] = useState(false);
  const [minDuration, setMinDuration] = useState("");
  // An absolute `?from=&to=` (e.g. a metric chart's "traces in this window" deep
  // link, JEF-433) overrides the picker's relative range so a chart's exact
  // moment is reachable even for a window the picker's presets can't express.
  const [urlParams] = useSearchParams();
  const absFrom = urlParams.get("from");
  const absTo = urlParams.get("to");

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
        ...(absFrom ? { from: absFrom, to: absTo ?? undefined } : rangeParams(rangeKey)),
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
  }, [service, name, attr, errorsOnly, minDuration, rangeKey, tick, absFrom, absTo]);

  const sort = useSort(traces, "start_time");
  const { sorted } = sort;
  // First-run (no data, no filters) reads differently from a filtered miss.
  const filtered = Boolean(name || attr || errorsOnly || minDuration || service);

  const filters = (
    <div className="filters">
      <input
        aria-label="Filter by root span name"
        placeholder="root span name…"
        value={name}
        onChange={(e) => setName(e.target.value)}
      />
      <input
        aria-label="Filter to traces with a span attribute, e.g. http.method=GET"
        placeholder="attribute key=value"
        value={attr}
        onChange={(e) => setAttr(e.target.value)}
        title="Filter to traces with a span attribute, e.g. http.method=GET"
      />
      <input
        type="number"
        min={0}
        aria-label="Only traces at least this many milliseconds long"
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
        <p className="muted">
          {filtered ? "No traces match these filters." : firstRunHint("traces")}
        </p>
      )}
      {traces.length > 0 && (
        <table>
          <thead>
            <tr>
              <SortHeader sort={sort} field="service" label="Service" />
              <SortHeader sort={sort} field="root_name" label="Root span" />
              <SortHeader sort={sort} field="start_time" label="Started" />
              <SortHeader sort={sort} field="duration_ms" label="Duration" num />
              <SortHeader sort={sort} field="span_count" label="Spans" num />
              <SortHeader sort={sort} field="error_count" label="Errors" num />
            </tr>
          </thead>
          <tbody>
            {sorted.map((t) => (
              <tr key={t.trace_id} className="clickable">
                <td>
                  <Link className="rowlink" to={to(t.trace_id)}>
                    {t.service ?? "—"}
                  </Link>
                </td>
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
