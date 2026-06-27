import { useEffect, useState } from "react";
import { Link } from "react-router-dom";
import { getTrace, type SpanRow } from "../api";
import { fmtDuration } from "./TraceList";

interface Row {
  span: SpanRow;
  depth: number;
}

const SPAN_KINDS = ["unspecified", "internal", "server", "client", "producer", "consumer"];

// Epoch microseconds from an RFC3339 timestamp. JS `Date` resolves only to ms,
// so we add the sub-ms digits (our span timestamps carry microsecond precision).
// The waterfall must derive a bar's offset AND width from this same function:
// using ms-truncated offsets with sub-ms widths desyncs a bar's start from its
// end and makes parent bars fail to enclose their children on short traces.
function epochMicros(iso: string): number {
  const ms = Date.parse(iso);
  const frac = /\.(\d+)/.exec(iso);
  const subMs = frac ? Number(frac[1].padEnd(6, "0").slice(0, 6)) % 1000 : 0;
  return ms * 1000 + subMs;
}

// Detail panel for a selected span: identity, status, timing, and attributes.
function SpanDetail({ span, onClose }: { span: SpanRow; onClose: () => void }) {
  const attrs = Object.entries(span.attributes ?? {});
  const fmt = (v: unknown) => (typeof v === "string" ? v : JSON.stringify(v));
  return (
    <div className="span-detail">
      <div className="span-detail-head">
        <strong className="mono">{span.name}</strong>
        <button className="xlink" onClick={onClose}>
          close
        </button>
      </div>
      <table className="kv">
        <tbody>
          <tr>
            <td className="muted">service</td>
            <td>{span.service ?? "—"}</td>
          </tr>
          <tr>
            <td className="muted">kind</td>
            <td>{span.kind != null ? (SPAN_KINDS[span.kind] ?? span.kind) : "—"}</td>
          </tr>
          <tr>
            <td className="muted">status</td>
            <td className={span.status_code === 2 ? "err" : ""}>
              {span.status_code === 2 ? "ERROR" : span.status_code === 1 ? "OK" : "unset"}
              {span.status_message ? ` · ${span.status_message}` : ""}
            </td>
          </tr>
          <tr>
            <td className="muted">duration</td>
            <td>{fmtDuration(span.duration_ms)}</td>
          </tr>
          <tr>
            <td className="muted">span id</td>
            <td className="mono">{span.span_id}</td>
          </tr>
          {span.parent_span_id && (
            <tr>
              <td className="muted">parent</td>
              <td className="mono">{span.parent_span_id}</td>
            </tr>
          )}
          {attrs.map(([k, v]) => (
            <tr key={k}>
              <td className="muted mono">{k}</td>
              <td className="mono body">{fmt(v)}</td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

function buildOrder(spans: SpanRow[]): Row[] {
  const byId = new Map(spans.map((s) => [s.span_id, s]));
  const children = new Map<string, SpanRow[]>();
  const roots: SpanRow[] = [];
  for (const s of spans) {
    if (s.parent_span_id && byId.has(s.parent_span_id)) {
      const arr = children.get(s.parent_span_id) ?? [];
      arr.push(s);
      children.set(s.parent_span_id, arr);
    } else {
      roots.push(s);
    }
  }
  const byStart = (a: SpanRow, b: SpanRow) => a.start_time.localeCompare(b.start_time);
  const out: Row[] = [];
  const visit = (s: SpanRow, depth: number) => {
    out.push({ span: s, depth });
    for (const c of (children.get(s.span_id) ?? []).sort(byStart)) visit(c, depth + 1);
  };
  for (const r of roots.sort(byStart)) visit(r, 0);
  return out;
}

export default function TraceWaterfall({
  traceId,
  onBack,
}: {
  traceId: string;
  onBack: () => void;
}) {
  const [spans, setSpans] = useState<SpanRow[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [selected, setSelected] = useState<string | null>(null);

  useEffect(() => {
    let active = true;
    setSelected(null);
    getTrace(traceId)
      .then((s) => {
        if (active) {
          setSpans(s);
          setError(null);
        }
      })
      .catch((e: unknown) => active && setError(String(e)));
    return () => {
      active = false;
    };
  }, [traceId]);

  if (error) return <p className="error">Failed to load trace: {error}</p>;
  if (spans.length === 0) return <p className="muted">Loading trace…</p>;

  const rows = buildOrder(spans);
  const t0 = Math.min(...spans.map((s) => epochMicros(s.start_time)));
  const t1 = Math.max(...spans.map((s) => epochMicros(s.end_time)));
  const total = Math.max(t1 - t0, 1);

  return (
    <div className="waterfall">
      <button className="back" onClick={onBack}>
        ← traces
      </button>
      <h2>
        {spans[0].service ?? "trace"} · <code>{traceId.slice(0, 16)}…</code> ·{" "}
        {fmtDuration(total)} ·{" "}
        <Link className="xlink" to={`/logs?trace_id=${traceId}`}>
          logs ↗
        </Link>
      </h2>
      <div className="bars">
        {rows.map(({ span, depth }) => {
          const start = epochMicros(span.start_time);
          // Width from end−start (same basis as `left`), not duration_ms, so the
          // bar's right edge always maps to the span's true end → parents enclose
          // children. duration_ms is still shown as the numeric label below.
          const left = ((start - t0) / total) * 100;
          const width = Math.max(((epochMicros(span.end_time) - start) / total) * 100, 0.4);
          const err = span.status_code === 2;
          const sel = span.span_id === selected;
          return (
            <div
              className={"bar-row clickable" + (sel ? " selected" : "")}
              key={span.span_id}
              onClick={() => setSelected(sel ? null : span.span_id)}
            >
              <div className="bar-label" style={{ paddingLeft: depth * 14 }} title={span.name}>
                {span.name}
              </div>
              <div className="bar-track">
                <div
                  className={"bar" + (err ? " err" : "")}
                  style={{ left: `${left}%`, width: `${width}%` }}
                  title={span.status_message ?? span.name}
                />
              </div>
              <div className="bar-dur num">{fmtDuration(span.duration_ms)}</div>
            </div>
          );
        })}
      </div>
      {selected && (
        <SpanDetail
          span={spans.find((s) => s.span_id === selected)!}
          onClose={() => setSelected(null)}
        />
      )}
    </div>
  );
}
