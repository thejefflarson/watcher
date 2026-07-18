import { Fragment, useEffect, useState } from "react";
import { Link, useSearchParams } from "react-router-dom";
import { listLogs, type LogRow } from "../api";
import { focusLabel } from "../focus";
import { useControls, rangeParams } from "../timerange";

function sevClass(n: number | null): string {
  if (n === null) return "";
  if (n >= 17) return "sev-error";
  if (n >= 13) return "sev-warn";
  if (n >= 9) return "sev-info";
  return "sev-debug";
}

const fmtAttr = (v: unknown) => (typeof v === "string" ? v : JSON.stringify(v));

export default function LogView() {
  const [logs, setLogs] = useState<LogRow[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [loaded, setLoaded] = useState(false);
  const [q, setQ] = useState("");
  const [attr, setAttr] = useState("");
  const [expanded, setExpanded] = useState<Set<number>>(new Set());
  // Service focus is global (header select, mirrored to `?service=`).
  const { rangeKey, service, tick } = useControls();
  const [params, setParams] = useSearchParams();
  const traceFilter = params.get("trace_id");
  const spanFilter = params.get("span_id");

  // Drop the given scope keys but keep everything else (crucially the global
  // `?service=` focus, so clearing a trace/span scope doesn't lose the service).
  const dropParams = (...keys: string[]) => {
    const next = new URLSearchParams(params);
    for (const k of keys) next.delete(k);
    setParams(next, { replace: true });
  };

  useEffect(() => {
    let active = true;
    const handle = setTimeout(() => {
      listLogs({
        q: q || undefined,
        service: service || undefined,
        trace_id: traceFilter ?? undefined,
        span_id: spanFilter ?? undefined,
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
        .catch((e: unknown) => active && setError(String(e)))
        .finally(() => active && setLoaded(true));
    }, 250);
    return () => {
      active = false;
      clearTimeout(handle);
    };
  }, [q, service, attr, traceFilter, spanFilter, rangeKey, tick]);

  const toggle = (id: number) =>
    setExpanded((prev) => {
      const next = new Set(prev);
      next.has(id) ? next.delete(id) : next.add(id);
      return next;
    });

  // Preserve the service focus when drilling into a trace/span from a log row.
  const svc = service ? `service=${encodeURIComponent(service)}` : "";
  const traceLink = (l: LogRow) => `/traces/${l.trace_id}${svc ? `?${svc}` : ""}`;
  const spanInTraceLink = (l: LogRow) => {
    const parts = [l.span_id ? `span=${l.span_id}` : "", svc].filter(Boolean);
    return `/traces/${l.trace_id}${parts.length ? `?${parts.join("&")}` : ""}`;
  };

  const emptyMessage = spanFilter
    ? "No logs recorded for this span."
    : traceFilter
      ? "No logs for this trace."
      : `No logs${focusLabel(service, rangeKey)}.`;

  return (
    <div className="logs">
      <div className="filters">
        <input
          placeholder="search body…"
          value={q}
          onChange={(e) => setQ(e.target.value)}
        />
        <input
          placeholder="attribute key=value"
          value={attr}
          onChange={(e) => setAttr(e.target.value)}
          title="Filter by an attribute, e.g. k8s.pod.name=api-7f"
        />
      </div>
      {(traceFilter || spanFilter) && (
        <p className="filter-banner">
          {spanFilter ? (
            <>
              logs for span <code>{spanFilter.slice(0, 16)}…</code>
              {traceFilter && (
                <>
                  {" in trace "}
                  <code>{traceFilter.slice(0, 16)}…</code>
                </>
              )}{" "}
              <button className="xlink" onClick={() => dropParams("span_id")}>
                widen to trace ↗
              </button>{" "}
            </>
          ) : (
            <>
              logs for trace <code>{traceFilter!.slice(0, 16)}…</code>{" "}
            </>
          )}
          <button
            className="xlink"
            onClick={() => dropParams("trace_id", "span_id")}
          >
            clear
          </button>
        </p>
      )}
      {error && <p className="error">Failed to load: {error}</p>}
      {!loaded && !error && <p className="muted">Loading…</p>}
      {loaded && logs.length === 0 && !error && (
        <p className="muted">{emptyMessage}</p>
      )}
      {logs.length > 0 && (
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
            {logs.map((l) => {
              const attrs = Object.entries(l.attributes ?? {});
              const open = expanded.has(l.id);
              return (
                <Fragment key={l.id}>
                  <tr className="clickable" onClick={() => toggle(l.id)}>
                    <td className="mono">
                      <button
                        className="expander"
                        onClick={(e) => {
                          e.stopPropagation();
                          toggle(l.id);
                        }}
                        title="Attributes"
                        aria-expanded={open}
                      >
                        {open ? "▾" : "▸"}
                      </button>
                      {new Date(l.time).toLocaleTimeString()}
                    </td>
                    <td>
                      <span className={"sev " + sevClass(l.severity_number)}>
                        {l.severity_text ?? l.severity_number ?? "—"}
                      </span>
                    </td>
                    <td>{l.service ?? "—"}</td>
                    <td className="mono">
                      {l.trace_id ? (
                        <Link
                          className="xlink"
                          to={`/traces/${l.trace_id}`}
                          title={l.trace_id}
                          onClick={(e) => e.stopPropagation()}
                        >
                          {l.trace_id.slice(0, 8)}
                        </Link>
                      ) : (
                        <span className="muted">—</span>
                      )}
                    </td>
                    <td className="mono body">{l.body ?? ""}</td>
                  </tr>
                  {open && (
                    <tr className="subseries-row">
                      <td colSpan={5}>
                        <div className="log-detail">
                          <div className="log-detail-links">
                            {l.trace_id && (
                              <Link className="xlink" to={traceLink(l)}>
                                trace ↗
                              </Link>
                            )}
                            {l.trace_id && l.span_id && (
                              <Link className="xlink" to={spanInTraceLink(l)}>
                                span in trace ↗
                              </Link>
                            )}
                          </div>
                          {attrs.length > 0 ? (
                            <table className="kv">
                              <tbody>
                                {attrs.map(([k, v]) => (
                                  <tr key={k}>
                                    <td className="muted mono">{k}</td>
                                    <td className="mono body">{fmtAttr(v)}</td>
                                    <td>
                                      <button
                                        className="xlink"
                                        onClick={() => setAttr(`${k}=${fmtAttr(v)}`)}
                                        title="Filter logs to this attribute"
                                      >
                                        filter: {k}={fmtAttr(v)}
                                      </button>
                                    </td>
                                  </tr>
                                ))}
                              </tbody>
                            </table>
                          ) : (
                            <p className="muted small">No attributes.</p>
                          )}
                        </div>
                      </td>
                    </tr>
                  )}
                </Fragment>
              );
            })}
          </tbody>
        </table>
      )}
    </div>
  );
}
