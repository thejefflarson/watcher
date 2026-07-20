import { useEffect, useMemo, useState } from "react";
import { Link } from "react-router-dom";
import {
  listAlerts,
  listAlertEvents,
  type AlertRule,
  type AlertEvent,
} from "../api";
import { formatValue } from "../format";
import { useControls } from "../timerange";

const fmtTime = (s: string) => new Date(s).toLocaleString();

// Coarse "12m" — honest since it is derived from event timestamps, not a declared
// sustained-condition (that's a separate rule feature).
function humanizeSince(fromMs: number, now: number): string {
  const s = Math.max(0, Math.floor((now - fromMs) / 1000));
  if (s < 60) return `${s}s`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}m`;
  const h = Math.floor(m / 60);
  if (h < 24) return `${h}h`;
  return `${Math.floor(h / 24)}d`;
}

// Deep-link to the metric chart the rule watches, carrying the threshold and the
// open firing window as overlay params (read by MetricChart). kind/unit pick the
// right chart type — histogram alerts must not land on a line chart.
export function chartHref(r: AlertRule, firingFrom: string | null): string {
  const p = new URLSearchParams();
  if (r.service) p.set("service", r.service);
  if (r.unit) p.set("unit", r.unit);
  if (r.kind) p.set("kind", r.kind);
  p.set("threshold", String(r.threshold));
  if (firingFrom) p.set("firing_from", firingFrom);
  return `/metrics/${encodeURIComponent(r.metric)}?${p.toString()}`;
}

// One red tick per fire over the last N days — a small-multiple of a rule's breach
// history, positioned by time on a hairline baseline.
const STRIP_DAYS = 7;
function BreachStrip({ fires, now }: { fires: number[]; now: number }) {
  const w = 130;
  const h = 14;
  const t0 = now - STRIP_DAYS * 86_400_000;
  const inWindow = fires.filter((t) => t >= t0);
  if (inWindow.length === 0) return <span className="muted">—</span>;
  const span = now - t0 || 1;
  return (
    <svg
      className="breach-strip"
      width={w}
      height={h}
      role="img"
      aria-label={`${inWindow.length} fired in the last ${STRIP_DAYS} days`}
    >
      <line x1={0} y1={h - 1} x2={w} y2={h - 1} stroke="var(--rule)" />
      {inWindow.map((t, i) => {
        const x = ((t - t0) / span) * w;
        return (
          <line key={i} x1={x} y1={1} x2={x} y2={h - 2} stroke="var(--error)" />
        );
      })}
    </svg>
  );
}

export default function Alerts() {
  const [rules, setRules] = useState<AlertRule[]>([]);
  const [events, setEvents] = useState<AlertEvent[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [loaded, setLoaded] = useState(false);
  const { tick } = useControls();

  const reload = () => {
    Promise.all([listAlerts(), listAlertEvents()])
      .then(([r, e]) => {
        setRules(r);
        setEvents(e);
        setError(null);
      })
      .catch((e: unknown) => setError(String(e)))
      .finally(() => setLoaded(true));
  };

  // eslint-disable-next-line react-hooks/exhaustive-deps
  useEffect(reload, [tick]);

  const now = Date.now();

  // Per-rule fire timestamps (for the breach strip) and the oldest open fire (for
  // the "for" column), grouped from the flat event list once.
  const byRule = useMemo(() => {
    const fires = new Map<number, number[]>();
    const openSince = new Map<number, number>();
    for (const e of events) {
      const t = new Date(e.fired_at).getTime();
      const arr = fires.get(e.rule_id) ?? [];
      arr.push(t);
      fires.set(e.rule_id, arr);
      if (!e.resolved_at) {
        openSince.set(e.rule_id, Math.min(openSince.get(e.rule_id) ?? t, t));
      }
    }
    return { fires, openSince };
  }, [events]);

  // Firing sorts to the top, then alphabetical — "what's on fire" first.
  const sortedRules = useMemo(
    () =>
      [...rules].sort(
        (a, b) =>
          Number(b.firing) - Number(a.firing) || a.name.localeCompare(b.name),
      ),
    [rules],
  );

  if (error) return <p className="error">Failed to load: {error}</p>;

  return (
    <div className="alerts">
      <p className="muted">
        Alert rules are managed declaratively in the chart's values
        (<code>server.alerts</code>) and reconciled on deploy — this view is
        read-only.
      </p>

      <h3>Rules</h3>
      {!loaded ? (
        <p className="muted">Loading…</p>
      ) : rules.length === 0 ? (
        <p className="muted">No alert rules configured.</p>
      ) : (
        <table>
          <thead>
            <tr>
              <th>state</th>
              <th>name</th>
              <th>condition</th>
              <th>for</th>
              <th>window</th>
              <th>last {STRIP_DAYS}d</th>
            </tr>
          </thead>
          <tbody>
            {sortedRules.map((r) => {
              const openMs = byRule.openSince.get(r.id);
              const openIso =
                openMs !== undefined ? new Date(openMs).toISOString() : null;
              return (
                <tr key={r.id} className="clickable">
                  <td
                    className={
                      r.firing ? "sev-error" : r.enabled ? "muted" : "disabled"
                    }
                  >
                    {r.firing ? "FIRING" : r.enabled ? "ok" : "disabled"}
                  </td>
                  <td>
                    <Link
                      className="rowlink"
                      to={chartHref(r, openIso)}
                      title={`Open ${r.metric} chart`}
                    >
                      {r.name}
                    </Link>
                  </td>
                  <td className="mono">
                    {r.agg}({r.metric}
                    {r.service ? `, ${r.service}` : ""}){" "}
                    {r.comparator === "gt" ? ">" : "<"} {r.threshold}
                  </td>
                  <td className={r.firing ? "sev-error" : "muted"}>
                    {r.firing && openMs !== undefined
                      ? `for ${humanizeSince(openMs, now)}`
                      : "—"}
                  </td>
                  <td className="muted">{r.window_secs}s</td>
                  <td>
                    <BreachStrip fires={byRule.fires.get(r.id) ?? []} now={now} />
                  </td>
                </tr>
              );
            })}
          </tbody>
        </table>
      )}

      <h3>Recent events</h3>
      {!loaded ? (
        <p className="muted">Loading…</p>
      ) : events.length === 0 ? (
        <p className="muted">No alerts have fired.</p>
      ) : (
        <table>
          <thead>
            <tr>
              <th>rule</th>
              <th>metric</th>
              <th className="num">value</th>
              <th>fired</th>
              <th>resolved</th>
            </tr>
          </thead>
          <tbody>
            {events.map((e) => (
              <tr key={e.id}>
                <td>{e.rule_name}</td>
                <td className="mono">{e.metric}</td>
                <td className="num">{formatValue(e.value, e.unit)}</td>
                <td className="muted">{fmtTime(e.fired_at)}</td>
                <td className={e.resolved_at ? "muted" : "sev-error"}>
                  {e.resolved_at ? fmtTime(e.resolved_at) : "firing"}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </div>
  );
}
