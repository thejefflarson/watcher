import { useEffect, useState } from "react";
import {
  listAlerts,
  listAlertEvents,
  type AlertRule,
  type AlertEvent,
} from "../api";
import { formatValue } from "../format";
import { useControls } from "../timerange";

const fmtTime = (s: string) => new Date(s).toLocaleString();

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
              <th>window</th>
            </tr>
          </thead>
          <tbody>
            {rules.map((r) => (
              <tr key={r.id}>
                <td className={r.firing ? "sev-error" : "muted"}>
                  {r.firing ? "FIRING" : r.enabled ? "ok" : "disabled"}
                </td>
                <td>{r.name}</td>
                <td className="mono">
                  {r.agg}({r.metric}
                  {r.service ? `, ${r.service}` : ""}) {r.comparator === "gt" ? ">" : "<"}{" "}
                  {r.threshold}
                </td>
                <td className="muted">{r.window_secs}s</td>
              </tr>
            ))}
          </tbody>
        </table>
      )}

      <h3>Recent events</h3>
      {!loaded ? (
        <p className="muted">Loading…</p>
      ) : events.length === 0 ? (
        <p className="muted">No events yet.</p>
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
                <td className="num">{formatValue(e.value)}</td>
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
