import { useEffect, useState } from "react";
import { Link, useSearchParams } from "react-router-dom";
import { listLogs, type LogRow } from "../api";
import { useControls, rangeParams } from "../timerange";

function sevClass(n: number | null): string {
  if (n === null) return "";
  if (n >= 17) return "sev-error";
  if (n >= 13) return "sev-warn";
  if (n >= 9) return "sev-info";
  return "sev-debug";
}

export default function LogView() {
  const [logs, setLogs] = useState<LogRow[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [q, setQ] = useState("");
  const [service, setService] = useState("");
  const [attr, setAttr] = useState("");
  const { rangeKey, tick } = useControls();
  const [params, setParams] = useSearchParams();
  const traceFilter = params.get("trace_id");

  useEffect(() => {
    let active = true;
    const handle = setTimeout(() => {
      listLogs({
        q: q || undefined,
        service: service || undefined,
        trace_id: traceFilter ?? undefined,
        attr: attr.includes("=") ? attr : undefined,
        limit: 200,
        ...rangeParams(rangeKey),
      })
        .then((l) => {
          if (active) {
            setLogs(l);
            setError(null);
          }
        })
        .catch((e: unknown) => active && setError(String(e)));
    }, 250);
    return () => {
      active = false;
      clearTimeout(handle);
    };
  }, [q, service, attr, traceFilter, rangeKey, tick]);

  return (
    <div className="logs">
      <div className="filters">
        <input
          placeholder="search body…"
          value={q}
          onChange={(e) => setQ(e.target.value)}
        />
        <input
          placeholder="service"
          value={service}
          onChange={(e) => setService(e.target.value)}
        />
        <input
          placeholder="attribute key=value"
          value={attr}
          onChange={(e) => setAttr(e.target.value)}
          title="Filter by an attribute, e.g. k8s.pod.name=api-7f"
        />
      </div>
      {traceFilter && (
        <p className="filter-banner">
          logs for trace <code>{traceFilter.slice(0, 16)}…</code>{" "}
          <button className="xlink" onClick={() => setParams({})}>
            clear
          </button>
        </p>
      )}
      {error && <p className="error">Failed to load: {error}</p>}
      {logs.length === 0 && !error && <p className="muted">No logs.</p>}
      <table>
        <thead>
          <tr>
            <th>Time</th>
            <th>Severity</th>
            <th>Service</th>
            <th>Trace</th>
            <th>Body</th>
          </tr>
        </thead>
        <tbody>
          {logs.map((l) => (
            <tr key={l.id}>
              <td className="mono">{new Date(l.time).toLocaleTimeString()}</td>
              <td>
                <span className={"sev " + sevClass(l.severity_number)}>
                  {l.severity_text ?? l.severity_number ?? "—"}
                </span>
              </td>
              <td>{l.service ?? "—"}</td>
              <td className="mono">
                {l.trace_id ? (
                  <Link className="xlink" to={`/traces/${l.trace_id}`} title={l.trace_id}>
                    {l.trace_id.slice(0, 8)}
                  </Link>
                ) : (
                  <span className="muted">—</span>
                )}
              </td>
              <td className="mono body">{l.body ?? ""}</td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}
