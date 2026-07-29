// First-run empty states name the OTLP ingest endpoint so a fresh install knows
// exactly where to point its exporter. Centralised so the port/path
// wording lives in one place rather than copy-pasted per signal.
export function firstRunHint(
  signal: "traces" | "logs" | "metrics",
  noun: string = signal,
): string {
  return `No ${noun} yet — point your OTLP exporter at :4318/v1/${signal}.`;
}
