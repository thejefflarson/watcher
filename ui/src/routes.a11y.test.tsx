// Runtime a11y route-smoke (JEF-442).
//
// JEF-431 gave us the STATIC a11y floor (eslint-plugin-jsx-a11y). This is the
// RUNTIME counterpart: mount each top-level route with a mocked API so it renders
// its loaded state, then run axe-core over the rendered tree. Static linting can't
// see ARIA that only resolves against the live tree, focus/role structure of a
// populated table, or a live region's wiring — this can.
//
// SCOPE / jsdom tradeoff (DECISION): the smoke runs in jsdom, not a real browser.
// jsdom has no layout engine and no canvas, so axe's layout-dependent checks —
// most notably `color-contrast` — cannot be evaluated here and are reported as
// "incomplete", never "pass". This smoke therefore asserts only STRUCTURAL rules
// (roles, names, ARIA relationships, labels) at serious/critical impact. It does
// NOT and cannot certify color contrast; that would need a headless browser
// (Playwright). jsdom is the pragmatic default per the ticket — the structural
// coverage is the high-value, cheap-on-CI part.
//
// The assertion is on axe violations by IMPACT, never on specific role/element
// markup, so it stays green regardless of in-flight row-markup changes (e.g.
// JEF-443's Alerts row rework).
import { describe, expect, it, vi } from "vitest";
import { render, screen } from "@testing-library/react";
import { MemoryRouter } from "react-router";
import type { ReactElement } from "react";
import { axe } from "vitest-axe";
import App from "./App";
import { TimeRangeProvider } from "./timerange";

// --- API mock -------------------------------------------------------------
// A full manual mock of the API layer so no route touches the network. Each
// function resolves a small-but-representative fixture so the route renders its
// LOADED state (populated tables/charts) — the state worth scanning — rather than
// its "Loading…" or empty placeholder.
vi.mock("./api", () => {
  const iso = "2026-07-18T12:00:00.000Z";
  const iso2 = "2026-07-18T12:01:00.000Z";

  const traces = [
    {
      trace_id: "abc123def456",
      service: "api",
      root_name: "GET /checkout",
      start_time: iso,
      end_time: iso2,
      duration_ms: 42.5,
      span_count: 3,
      error_count: 1,
    },
  ];

  const spans = [
    {
      trace_id: "abc123def456",
      span_id: "1111111111111111",
      parent_span_id: null,
      service: "api",
      name: "GET /checkout",
      kind: 2,
      start_time: iso,
      end_time: iso2,
      duration_ms: 42.5,
      status_code: 2,
      status_message: "boom",
      attributes: { "http.method": "GET" },
    },
    {
      trace_id: "abc123def456",
      span_id: "2222222222222222",
      parent_span_id: "1111111111111111",
      service: "api",
      name: "db.query",
      kind: 3,
      start_time: iso,
      end_time: iso2,
      duration_ms: 20.1,
      status_code: 1,
      status_message: null,
      attributes: {},
    },
  ];

  const logs = [
    {
      id: 1,
      time: iso,
      trace_id: "abc123def456",
      span_id: "1111111111111111",
      service: "api",
      severity_number: 17,
      severity_text: "ERROR",
      body: "checkout failed",
      attributes: { "k8s.pod.name": "api-7f" },
    },
  ];

  const metrics = [
    {
      name: "http_requests",
      service: "api",
      kind: "gauge",
      unit: "1",
      last_time: iso2,
      last_value: 12,
      spark: [10, 11, 12],
      count_spark: null,
      dist: null,
      series_count: 1,
    },
  ];

  const facet = {
    kind: "gauge",
    rated: false,
    unit: "1",
    series: [
      {
        attrs: { pod: "api-7f" },
        points: [
          { t: iso, v: 10 },
          { t: iso2, v: 12 },
        ],
      },
    ],
    truncated: 0,
  };

  const serviceMap = {
    nodes: ["api", "db"],
    edges: [{ source: "api", target: "db", calls: 5 }],
  };

  const services = [
    {
      service: "api",
      spans: 100,
      errors: 2,
      error_rate: 0.02,
      p50_ms: 12,
      p95_ms: 88,
      p99_ms: 140,
    },
  ];

  const alerts = [
    {
      id: 1,
      name: "high latency",
      metric: "http_p95",
      service: "api",
      comparator: "gt",
      threshold: 100,
      agg: "avg",
      window_secs: 300,
      enabled: true,
      created_at: iso,
      firing: true,
      kind: "gauge",
      unit: "ms",
    },
  ];

  const alertEvents = [
    {
      id: 1,
      rule_id: 1,
      rule_name: "high latency",
      metric: "http_p95",
      value: 140,
      fired_at: iso,
      resolved_at: null,
      kind: "gauge",
      unit: "ms",
    },
  ];

  const histogram = { bounds: [1, 2, 4], unit: "ms", buckets: [] };

  return {
    listTraces: vi.fn().mockResolvedValue(traces),
    getTrace: vi.fn().mockResolvedValue(spans),
    listLogs: vi.fn().mockResolvedValue(logs),
    listMetrics: vi.fn().mockResolvedValue(metrics),
    getMetricSeries: vi.fn().mockResolvedValue([]),
    getMetricExemplars: vi.fn().mockResolvedValue([]),
    getMetricDims: vi.fn().mockResolvedValue([]),
    getMetricSeriesGrouped: vi.fn().mockResolvedValue([]),
    getMetricFacet: vi.fn().mockResolvedValue(facet),
    getMetricHistogram: vi.fn().mockResolvedValue(histogram),
    getMetricHistFacet: vi
      .fn()
      .mockResolvedValue({ unit: "ms", bounds: [], series: [], truncated: 0 }),
    getServiceMap: vi.fn().mockResolvedValue(serviceMap),
    listServices: vi.fn().mockResolvedValue(services),
    listAlerts: vi.fn().mockResolvedValue(alerts),
    listAlertEvents: vi.fn().mockResolvedValue(alertEvents),
  };
});

// Mount the whole app (nav + controls + the matched route) at a URL so the scan
// covers the chrome too, not just the route body.
function renderAt(path: string): ReactElement {
  return (
    <MemoryRouter initialEntries={[path]}>
      <TimeRangeProvider>
        <App />
      </TimeRangeProvider>
    </MemoryRouter>
  );
}

// Fail only on serious/critical violations — the structural floor. Lower-impact
// findings (minor/moderate: heading-order, landmark-uniqueness, …) and layout
// rules jsdom can't evaluate (color-contrast → "incomplete") are out of scope for
// a cheap CI smoke and are not asserted here.
async function expectNoBlockingViolations(
  container: HTMLElement,
  ruleOverrides: Record<string, { enabled: boolean }> = {},
) {
  const results = await axe(container, {
    rules: {
      // Per-route waivers layer on top (kept as narrow as possible so a rule stays
      // enforced everywhere it isn't a documented pre-existing baseline).
      ...ruleOverrides,
    },
  });
  const blocking = results.violations.filter(
    (v) => v.impact === "serious" || v.impact === "critical",
  );
  // Pass the filtered result through the matcher so a failure prints the offending
  // rule, node, and help URL rather than a bare length mismatch.
  expect({ ...results, violations: blocking }).toHaveNoViolations();
}

describe("runtime axe route smoke", () => {
  it("traces list has no serious/critical a11y violations", async () => {
    const { container } = render(renderAt("/traces"));
    await screen.findByRole("table");
    await expectNoBlockingViolations(container);
  });

  it("trace detail / waterfall has no serious/critical a11y violations", async () => {
    const { container } = render(renderAt("/traces/abc123def456"));
    await screen.findByText("← traces");
    // Wait for the spans to render as pressable bar rows before scanning.
    await screen.findByRole("button", { name: /GET \/checkout/ });
    await expectNoBlockingViolations(container);
  });

  it("logs has no serious/critical a11y violations", async () => {
    const { container } = render(renderAt("/logs"));
    await screen.findByRole("table");
    await expectNoBlockingViolations(container);
  });

  it("metrics list has no serious/critical a11y violations", async () => {
    const { container } = render(renderAt("/metrics"));
    await screen.findByRole("table");
    await expectNoBlockingViolations(container);
  });

  it("metric detail / chart has no serious/critical a11y violations", async () => {
    const { container } = render(renderAt("/metrics/http_requests?service=api"));
    // The hand-drawn chart is an <svg role="img"> with an aria-label; it only
    // renders once the facet data resolves.
    await screen.findByRole("img");
    await expectNoBlockingViolations(container);
  });

  it("services has no serious/critical a11y violations", async () => {
    const { container } = render(renderAt("/services"));
    await screen.findByRole("table");
    await expectNoBlockingViolations(container);
  });

  it("service map has no serious/critical a11y violations", async () => {
    const { container } = render(renderAt("/map"));
    // The map is an <svg role="group"> labelled with the node/edge/call counts —
    // a grouping, NOT role="img", so its focusable node-buttons (JEF-431) are
    // legitimately interactive children. This resolves the pre-existing
    // `nested-interactive` violation that role="img" caused (JEF-455), so the
    // rule is now enforced here too — no per-route waiver.
    await screen.findByRole("group", { name: /Service map/ });
    await expectNoBlockingViolations(container);
  });

  it("alerts has no serious/critical a11y violations", async () => {
    const { container } = render(renderAt("/alerts"));
    // Alerts renders two tables (rules + recent events); wait for them, then
    // assert on axe impact — not on the row markup. JEF-443 reworks these rows in
    // parallel, so this must pass whichever version of Alerts.tsx is on main.
    await screen.findAllByRole("table");
    await expectNoBlockingViolations(container);
  });
});
