import { describe, expect, it } from "vitest";
import { logsHref, traceHref } from "./links";

// JEF-534: trace_id/span_id are ingested OTLP data (attacker-shapeable) and were
// interpolated raw into these deep-links, unlike the neighboring `service` focus
// which was already `encodeURIComponent`-ed. These tests pin the well-formed-id
// shape (no behavior change vs. the pre-fix raw interpolation) and lock the
// encoding for ids with special characters.
describe("traceHref", () => {
  it("links to just the trace for a well-formed hex id (no behavior change)", () => {
    expect(traceHref("abcdef0123456789")).toBe("/traces/abcdef0123456789");
  });

  it("carries the service focus", () => {
    expect(traceHref("abcdef0123456789", { service: "api" })).toBe(
      "/traces/abcdef0123456789?service=api",
    );
  });

  it("carries span + service, span first (matches the pre-fix param order)", () => {
    expect(traceHref("abcdef0123456789", { spanId: "0123456789abcdef", service: "api" })).toBe(
      "/traces/abcdef0123456789?span=0123456789abcdef&service=api",
    );
  });

  it("encodes a trace id with special characters into the path segment", () => {
    expect(traceHref("dead&beef#1")).toBe("/traces/dead%26beef%231");
  });

  it("encodes a span id with special characters into the query string", () => {
    expect(traceHref("t1", { spanId: "span&1" })).toBe("/traces/t1?span=span%261");
  });
});

describe("logsHref", () => {
  it("carries just the trace id when no span is given", () => {
    expect(logsHref("abcdef0123456789")).toBe("/logs?trace_id=abcdef0123456789");
  });

  it("carries trace_id and span_id for a well-formed hex id (no behavior change)", () => {
    expect(logsHref("abcdef0123456789", "0123456789abcdef")).toBe(
      "/logs?trace_id=abcdef0123456789&span_id=0123456789abcdef",
    );
  });

  it("encodes special characters instead of injecting an extra query param", () => {
    expect(logsHref("dead&beef", "span#1")).toBe(
      "/logs?trace_id=dead%26beef&span_id=span%231",
    );
  });
});
