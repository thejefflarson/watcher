import { useEffect, useState } from "react";
import { getTrace, type SpanRow } from "../api";
import { fmtDuration } from "./TraceList";

interface Row {
  span: SpanRow;
  depth: number;
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

  useEffect(() => {
    let active = true;
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
  const t0 = Math.min(...spans.map((s) => Date.parse(s.start_time)));
  const t1 = Math.max(...spans.map((s) => Date.parse(s.end_time)));
  const total = Math.max(t1 - t0, 1);

  return (
    <div className="waterfall">
      <button className="back" onClick={onBack}>
        ← traces
      </button>
      <h2>
        {spans[0].service ?? "trace"} · <code>{traceId.slice(0, 16)}…</code> ·{" "}
        {fmtDuration(total)}
      </h2>
      <div className="bars">
        {rows.map(({ span, depth }) => {
          const left = ((Date.parse(span.start_time) - t0) / total) * 100;
          const width = Math.max((span.duration_ms / total) * 100, 0.4);
          const err = span.status_code === 2;
          return (
            <div className="bar-row" key={span.span_id}>
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
    </div>
  );
}
