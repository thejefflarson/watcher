import { useEffect, useRef, useState } from "react";
import { Link, useSearchParams } from "react-router-dom";
import { getTrace, type SpanRow } from "../api";
import { rowKeyActivate } from "../a11y";
import { fmtDuration } from "./TraceList";

export interface Row {
  span: SpanRow;
  depth: number;
}

const SPAN_KINDS = ["unspecified", "internal", "server", "client", "producer", "consumer"];

// Epoch microseconds from an RFC3339 timestamp. JS `Date` resolves only to ms,
// so we add the sub-ms digits (our span timestamps carry microsecond precision).
// The waterfall must derive a bar's offset AND width from this same function:
// using ms-truncated offsets with sub-ms widths desyncs a bar's start from its
// end and makes parent bars fail to enclose their children on short traces.
export function epochMicros(iso: string): number {
  const ms = Date.parse(iso);
  const frac = /\.(\d+)/.exec(iso);
  const subMs = frac ? Number(frac[1].padEnd(6, "0").slice(0, 6)) % 1000 : 0;
  return ms * 1000 + subMs;
}

// Left/width as percentages of the trace's [t0, t0+total] window. Shared by the
// bars AND the minimap so the two stay pixel-for-pixel consistent — a span at a
// given x in one reads at the same x in the other.
export function barGeometry(
  span: SpanRow,
  t0: number,
  total: number,
): { left: number; width: number } {
  const start = epochMicros(span.start_time);
  // Width from end−start (same basis as `left`), not duration_ms, so the bar's
  // right edge always maps to the span's true end → parents enclose children.
  const width = Math.max(((epochMicros(span.end_time) - start) / total) * 100, 0.4);
  const left = ((start - t0) / total) * 100;
  return { left, width };
}

function buildTree(spans: SpanRow[]) {
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
  return { byId, children, roots };
}

export function buildOrder(spans: SpanRow[]): Row[] {
  const { children, roots } = buildTree(spans);
  const byStart = (a: SpanRow, b: SpanRow) => a.start_time.localeCompare(b.start_time);
  const out: Row[] = [];
  const visit = (s: SpanRow, depth: number) => {
    out.push({ span: s, depth });
    for (const c of (children.get(s.span_id) ?? []).sort(byStart)) visit(c, depth + 1);
  };
  for (const r of roots.sort(byStart)) visit(r, 0);
  return out;
}

export interface CriticalPath {
  onPath: Set<string>;
  criticalMs: number;
  skewed: boolean;
}

// Per-root critical path: at each node, descend into whichever child's subtree
// finishes latest (its own end, or a descendant's if later). A negative span
// duration (end before its own start) means the clocks feeding this trace
// disagree enough that "finishes last" can't be trusted — bail out rather than
// draw a path that looks authoritative but is really just noise.
export function computeCriticalPath(spans: SpanRow[]): CriticalPath {
  if (spans.some((s) => epochMicros(s.end_time) < epochMicros(s.start_time))) {
    return { onPath: new Set(), criticalMs: 0, skewed: true };
  }
  const { children, roots } = buildTree(spans);
  const subtreeEnd = new Map<string, number>();
  const endOf = (s: SpanRow): number => {
    const cached = subtreeEnd.get(s.span_id);
    if (cached !== undefined) return cached;
    let end = epochMicros(s.end_time);
    for (const c of children.get(s.span_id) ?? []) end = Math.max(end, endOf(c));
    subtreeEnd.set(s.span_id, end);
    return end;
  };
  for (const s of spans) endOf(s);

  const onPath = new Set<string>();
  let criticalMicros = 0;
  for (const root of roots) {
    let node: SpanRow | undefined = root;
    while (node) {
      onPath.add(node.span_id);
      const kids: SpanRow[] = children.get(node.span_id) ?? [];
      if (kids.length === 0) break;
      node = kids.reduce((slowest: SpanRow, c: SpanRow) =>
        subtreeEnd.get(c.span_id)! > subtreeEnd.get(slowest.span_id)! ? c : slowest,
      );
    }
    criticalMicros += subtreeEnd.get(root.span_id)! - epochMicros(root.start_time);
  }
  return { onPath, criticalMs: criticalMicros / 1000, skewed: false };
}

// Ancestors of a matched span stay visible even if their own name/service
// didn't match — otherwise a hit deep in the tree loses all its context.
export function withAncestors(spans: SpanRow[], matches: Set<string>): Set<string> {
  const byId = new Map(spans.map((s) => [s.span_id, s]));
  const visible = new Set(matches);
  for (const id of matches) {
    let cur = byId.get(id);
    while (cur?.parent_span_id) {
      const parent = byId.get(cur.parent_span_id);
      if (!parent || visible.has(parent.span_id)) break;
      visible.add(parent.span_id);
      cur = parent;
    }
  }
  return visible;
}

// Detail panel for a selected span: identity, status, timing, and attributes.
function SpanDetail({ span, onClose }: { span: SpanRow; onClose: () => void }) {
  const attrs = Object.entries(span.attributes ?? {});
  const fmt = (v: unknown) => (typeof v === "string" ? v : JSON.stringify(v));
  return (
    <div className="span-detail">
      <div className="span-detail-head">
        <strong className="mono">{span.name}</strong>
        {/* Cross-signal drill: this span's logs (trace_id + span_id), or the
            whole service's logs (which also sets the global service focus). */}
        <Link
          className="xlink"
          to={`/logs?trace_id=${span.trace_id}&span_id=${span.span_id}`}
        >
          logs for this span ↗
        </Link>
        {span.service && (
          <Link
            className="xlink"
            to={`/logs?service=${encodeURIComponent(span.service)}`}
          >
            logs for this service ↗
          </Link>
        )}
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

interface ScrollState {
  top: number;
  clientHeight: number;
  scrollHeight: number;
}

// A full-width, scaled-down replica of the bars list: one 1px tick per span at
// its true time offset (via the shared `barGeometry`), stacked top-to-bottom in
// row order. A translucent rect marks the currently-scrolled viewport; clicking
// anywhere jumps the bars list to that proportional row.
function Minimap({
  rows,
  t0,
  total,
  errorCount,
  scroll,
  onJump,
}: {
  rows: Row[];
  t0: number;
  total: number;
  errorCount: number;
  scroll: ScrollState;
  onJump: (fraction: number) => void;
}) {
  const H = 24;
  const viewTop = scroll.scrollHeight > 0 ? (scroll.top / scroll.scrollHeight) * H : 0;
  const viewHeight =
    scroll.scrollHeight > 0 ? Math.max((scroll.clientHeight / scroll.scrollHeight) * H, 2) : H;
  return (
    <svg
      className="minimap"
      width="100%"
      height={H}
      viewBox={`0 0 100 ${H}`}
      preserveAspectRatio="none"
      role="img"
      aria-label={`Trace overview: ${rows.length} spans, ${errorCount} error${errorCount === 1 ? "" : "s"}`}
      onClick={(e) => {
        const rect = e.currentTarget.getBoundingClientRect();
        onJump((e.clientY - rect.top) / rect.height);
      }}
    >
      {rows.map(({ span }, i) => {
        const { left, width } = barGeometry(span, t0, total);
        const y = (i / rows.length) * H;
        return (
          <line
            key={span.span_id}
            x1={left}
            x2={left + width}
            y1={y}
            y2={y}
            stroke={span.status_code === 2 ? "var(--error)" : "#555"}
          />
        );
      })}
      <rect className="minimap-viewport" x={0} y={viewTop} width={100} height={viewHeight} />
    </svg>
  );
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
  const [filterText, setFilterText] = useState("");
  const [errorsOnly, setErrorsOnly] = useState(false);
  const [criticalPathOverride, setCriticalPathOverride] = useState<boolean | null>(null);
  const [errorIdx, setErrorIdx] = useState(0);
  const [scroll, setScroll] = useState<ScrollState>({ top: 0, clientHeight: 0, scrollHeight: 0 });
  const barsRef = useRef<HTMLDivElement | null>(null);
  const rowRefs = useRef(new Map<string, HTMLDivElement>());
  const [params] = useSearchParams();
  // `?span=` opens straight to a span's detail — the "span in trace ↗" drill
  // from a log row lands here.
  const spanParam = params.get("span");

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

  // Sync the selected span from the URL (trace change clears it unless `?span=`).
  useEffect(() => {
    setSelected(spanParam ?? null);
  }, [traceId, spanParam]);

  // A new trace starts with a clean slate — a filter or a critical-path
  // override carried over from the last trace would be confusing here.
  useEffect(() => {
    setFilterText("");
    setErrorsOnly(false);
    setCriticalPathOverride(null);
    setErrorIdx(0);
  }, [traceId]);

  const updateScroll = () => {
    const el = barsRef.current;
    if (!el) return;
    setScroll({ top: el.scrollTop, clientHeight: el.clientHeight, scrollHeight: el.scrollHeight });
  };

  useEffect(updateScroll, [spans]);

  if (error) return <p className="error">Failed to load trace: {error}</p>;
  if (spans.length === 0) return <p className="muted">Loading trace…</p>;

  const rows = buildOrder(spans);
  const t0 = Math.min(...spans.map((s) => epochMicros(s.start_time)));
  const t1 = Math.max(...spans.map((s) => epochMicros(s.end_time)));
  const total = Math.max(t1 - t0, 1);

  const errorSpans = rows.filter((r) => r.span.status_code === 2).map((r) => r.span);
  const jumpToError = () => {
    if (errorSpans.length === 0) return;
    const next = errorSpans[errorIdx % errorSpans.length];
    setErrorIdx((i) => i + 1);
    setSelected(next.span_id);
    rowRefs.current.get(next.span_id)?.scrollIntoView({ block: "center", behavior: "smooth" });
  };

  const criticalPath = computeCriticalPath(spans);
  const criticalPathOn = criticalPathOverride ?? spans.length > 15;
  // `total` above is a microsecond delta (see epochMicros); convert to ms to
  // pair sensibly with `criticalPath.criticalMs`, which is already in ms.
  const totalMs = total / 1000;

  const q = filterText.trim().toLowerCase();
  const filterActive = q !== "" || errorsOnly;
  const matches = new Set<string>();
  for (const { span } of rows) {
    const textHit =
      !q || span.name.toLowerCase().includes(q) || (span.service ?? "").toLowerCase().includes(q);
    const errHit = !errorsOnly || span.status_code === 2;
    if (textHit && errHit) matches.add(span.span_id);
  }
  const visible = filterActive ? withAncestors(spans, matches) : null;

  const jumpToFraction = (fraction: number) => {
    const el = barsRef.current;
    if (!el) return;
    const target = fraction * el.scrollHeight - el.clientHeight / 2;
    el.scrollTo({
      top: Math.max(0, Math.min(target, el.scrollHeight - el.clientHeight)),
      behavior: "smooth",
    });
  };

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
        {errorSpans.length > 0 && (
          <>
            {" · "}
            <button className="xlink" onClick={jumpToError}>
              {errorSpans.length} error{errorSpans.length === 1 ? "" : "s"} · jump ↓
            </button>
          </>
        )}
      </h2>
      <div className="filters">
        <input
          aria-label="Filter spans by name or service"
          placeholder="filter spans…"
          value={filterText}
          onChange={(e) => setFilterText(e.target.value)}
        />
        <label className="checkbox">
          <input
            type="checkbox"
            checked={errorsOnly}
            onChange={(e) => setErrorsOnly(e.target.checked)}
          />
          errors only
        </label>
        <label className="checkbox">
          <input
            type="checkbox"
            checked={criticalPathOn}
            onChange={(e) => setCriticalPathOverride(e.target.checked)}
          />
          critical path
        </label>
        {filterActive && (
          <span className="muted small">
            {matches.size} of {rows.length} spans
          </span>
        )}
      </div>
      {criticalPathOn && (
        <p className="muted small">
          {criticalPath.skewed
            ? "critical path · clock skew"
            : `critical path · ${fmtDuration(criticalPath.criticalMs)} of ${fmtDuration(totalMs)}`}
        </p>
      )}
      {rows.length > 40 && (
        <Minimap
          rows={rows}
          t0={t0}
          total={total}
          errorCount={errorSpans.length}
          scroll={scroll}
          onJump={jumpToFraction}
        />
      )}
      <div
        className={"bars" + (rows.length > 40 ? " scrollable" : "")}
        ref={barsRef}
        onScroll={updateScroll}
      >
        {rows.map(({ span, depth }) => {
          const { left, width } = barGeometry(span, t0, total);
          const err = span.status_code === 2;
          const sel = span.span_id === selected;
          const onCriticalPath =
            criticalPathOn && !criticalPath.skewed && criticalPath.onPath.has(span.span_id);
          const dim = visible !== null && !visible.has(span.span_id);
          const toggle = () => setSelected(sel ? null : span.span_id);
          return (
            <div
              className={"bar-row" + (sel ? " selected" : "") + (dim ? " dim" : "")}
              key={span.span_id}
              ref={(el) => {
                if (el) rowRefs.current.set(span.span_id, el);
                else rowRefs.current.delete(span.span_id);
              }}
              role="button"
              tabIndex={0}
              aria-pressed={sel}
              aria-label={`${span.name}, ${fmtDuration(span.duration_ms)}${err ? ", error" : ""}`}
              onClick={toggle}
              onKeyDown={rowKeyActivate(toggle)}
            >
              <div
                className={"bar-label" + (err ? " err" : "")}
                style={{ paddingLeft: depth * 14 }}
                title={span.name}
              >
                {span.name}
              </div>
              <div className="bar-track">
                <div
                  className={"bar" + (err ? " err" : "") + (onCriticalPath ? " crit" : "")}
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
