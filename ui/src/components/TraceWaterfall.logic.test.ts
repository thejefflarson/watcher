import { describe, expect, it } from "vitest";
import type { SpanRow } from "../api";
import { fmtDuration } from "./TraceList";
import {
  barGeometry,
  buildOrder,
  computeCriticalPath,
  epochMicros,
  traceDurationMs,
  withAncestors,
} from "./TraceWaterfall";

// Same fixture shape as Alerts.chartHref.test.ts — a full SpanRow with sane
// defaults, overridden per test.
const span = (over: Partial<SpanRow> & { span_id: string }): SpanRow => ({
  trace_id: "t1",
  parent_span_id: null,
  service: "api",
  name: "op",
  kind: null,
  start_time: "2026-07-01T00:00:00.000000Z",
  end_time: "2026-07-01T00:00:00.010000Z",
  duration_ms: 10,
  status_code: null,
  status_message: null,
  attributes: {},
  ...over,
});

describe("barGeometry", () => {
  it("derives left/width from start/end, not duration_ms", () => {
    // A 500ms window; a span running from +100ms to +200ms sits at 20% width 20%.
    const t0 = epochMicros("2026-07-01T00:00:00.000000Z");
    const total = 500_000; // 500ms in micros
    const s = span({
      span_id: "a",
      start_time: "2026-07-01T00:00:00.100000Z",
      end_time: "2026-07-01T00:00:00.200000Z",
      duration_ms: 999, // deliberately wrong — geometry must ignore this
    });
    const { left, width } = barGeometry(s, t0, total);
    expect(left).toBeCloseTo(20);
    expect(width).toBeCloseTo(20);
  });

  it("floors width at 0.4% so instant spans stay visible", () => {
    const t0 = epochMicros("2026-07-01T00:00:00.000000Z");
    const s = span({ span_id: "a", start_time: "2026-07-01T00:00:00.000000Z", end_time: "2026-07-01T00:00:00.000000Z" });
    const { width } = barGeometry(s, t0, 500_000);
    expect(width).toBe(0.4);
  });
});

describe("traceDurationMs", () => {
  it("converts the trace's µs window to ms, matching what fmtDuration expects", () => {
    // The header used to pass the µs delta straight into fmtDuration
    // (which expects ms), overstating a 480ms trace as "480.00s" — ~1000×.
    const total = 480_000; // 480ms in micros, same basis as `total` in the component
    const ms = traceDurationMs(total);
    expect(ms).toBe(480);
    expect(fmtDuration(ms)).toBe("480.0ms");
    // The pre-fix bug: feeding the µs delta straight into fmtDuration would
    // render as seconds, ~1000× the true duration.
    expect(fmtDuration(total)).toBe("480.00s");
  });
});

describe("buildOrder", () => {
  it("orders a root before its children, sorted by start time", () => {
    const root = span({ span_id: "root", start_time: "2026-07-01T00:00:00.000000Z" });
    const child2 = span({
      span_id: "c2",
      parent_span_id: "root",
      start_time: "2026-07-01T00:00:00.002000Z",
    });
    const child1 = span({
      span_id: "c1",
      parent_span_id: "root",
      start_time: "2026-07-01T00:00:00.001000Z",
    });
    const rows = buildOrder([child2, root, child1]);
    expect(rows.map((r) => r.span.span_id)).toEqual(["root", "c1", "c2"]);
    expect(rows.map((r) => r.depth)).toEqual([0, 1, 1]);
  });
});

describe("computeCriticalPath", () => {
  it("follows the child whose subtree finishes last at each level", () => {
    // root
    //  ├─ fast (ends early)
    //  └─ slow (ends late) ── slower-grandchild (ends latest)
    const root = span({
      span_id: "root",
      start_time: "2026-07-01T00:00:00.000000Z",
      end_time: "2026-07-01T00:00:00.090000Z",
    });
    const fast = span({
      span_id: "fast",
      parent_span_id: "root",
      start_time: "2026-07-01T00:00:00.000000Z",
      end_time: "2026-07-01T00:00:00.020000Z",
    });
    const slow = span({
      span_id: "slow",
      parent_span_id: "root",
      start_time: "2026-07-01T00:00:00.000000Z",
      end_time: "2026-07-01T00:00:00.060000Z",
    });
    const grandchild = span({
      span_id: "grandchild",
      parent_span_id: "slow",
      start_time: "2026-07-01T00:00:00.060000Z",
      end_time: "2026-07-01T00:00:00.090000Z",
    });
    const result = computeCriticalPath([root, fast, slow, grandchild]);
    expect(result.skewed).toBe(false);
    expect(result.onPath).toEqual(new Set(["root", "slow", "grandchild"]));
    // root start (t0) to the latest-finishing descendant (grandchild's end, 90ms in).
    expect(result.criticalMs).toBeCloseTo(90);
  });

  it("bails out with skewed=true when a span's end precedes its own start", () => {
    const root = span({
      span_id: "root",
      start_time: "2026-07-01T00:00:00.000000Z",
      end_time: "2026-07-01T00:00:00.100000Z",
    });
    // Clock skew: this child's end is before its own start.
    const skewedChild = span({
      span_id: "child",
      parent_span_id: "root",
      start_time: "2026-07-01T00:00:00.050000Z",
      end_time: "2026-07-01T00:00:00.010000Z",
    });
    const result = computeCriticalPath([root, skewedChild]);
    expect(result.skewed).toBe(true);
    expect(result.onPath.size).toBe(0);
  });
});

describe("withAncestors", () => {
  it("keeps ancestors of a match visible even though they didn't match themselves", () => {
    const root = span({ span_id: "root" });
    const mid = span({ span_id: "mid", parent_span_id: "root" });
    const leaf = span({ span_id: "leaf", parent_span_id: "mid" });
    const sibling = span({ span_id: "sibling", parent_span_id: "root" });
    const visible = withAncestors([root, mid, leaf, sibling], new Set(["leaf"]));
    expect(visible).toEqual(new Set(["leaf", "mid", "root"]));
    expect(visible.has("sibling")).toBe(false);
  });
});
