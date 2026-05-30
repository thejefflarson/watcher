# 0009. Minimal, high-data-ink UI

- Status: Accepted
- Date: 2026-05-30

## Context

The first UI pass was a conventional "dark dashboard": boxed panels, bordered
rows, filled severity pills, uppercase headers, decorative accent color and emoji.
It read fine but spent a lot of ink on chrome rather than data.

## Decision

The UI follows Tufte's principles — maximize the data-ink ratio. Light ground,
near-black ink, **hairline rules instead of boxes**, no pills (severity is colored
text), no decorative color or emoji (color is reserved for data: errors, latency,
severity), tabular numerals. Metrics show **inline sparklines** (small multiples);
the service map is thin lines + small dots + plain labels.

## Consequences

- Dense, legible, fast to scan; the data carries the page.
- Plain React + SVG, no chart/graph libraries to maintain.
- It's deliberately austere — no theming, no dark mode. Trends are shown inline
  (sparklines), not as full interactive time-series charts yet (a known follow-up).
