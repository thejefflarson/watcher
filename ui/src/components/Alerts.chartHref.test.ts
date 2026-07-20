import { describe, expect, it } from "vitest";
import type { AlertRule } from "../api";
import { chartHref } from "./Alerts";

// The rules-table primary cell is a <Link to={chartHref(...)}> (JEF-443). These
// tests lock the exact deep-link target — same chart + threshold/firing overlay
// params the whole-row navigation built before — so the row-interaction refactor
// can't silently move where a rule points.
const rule = (over: Partial<AlertRule> = {}): AlertRule => ({
  id: 1,
  name: "p99 too high",
  metric: "http.server.duration",
  service: "api",
  comparator: "gt",
  threshold: 500,
  agg: "max",
  window_secs: 300,
  enabled: true,
  created_at: "2026-01-01T00:00:00Z",
  firing: false,
  kind: "histogram",
  unit: "ms",
  ...over,
});

describe("chartHref", () => {
  it("carries service/unit/kind/threshold and url-encodes the metric name", () => {
    const href = chartHref(rule(), null);
    expect(href).toBe(
      "/metrics/http.server.duration?service=api&unit=ms&kind=histogram&threshold=500",
    );
  });

  it("appends firing_from only when a firing window is open", () => {
    expect(chartHref(rule(), "2026-07-18T10:00:00.000Z")).toBe(
      "/metrics/http.server.duration?service=api&unit=ms&kind=histogram&threshold=500&firing_from=2026-07-18T10%3A00%3A00.000Z",
    );
  });

  it("omits optional params that are null and still emits the threshold", () => {
    const href = chartHref(
      rule({ service: null, unit: null, kind: null, threshold: 0 }),
      null,
    );
    expect(href).toBe("/metrics/http.server.duration?threshold=0");
  });
});
