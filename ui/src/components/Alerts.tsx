import { useEffect, useState } from "react";
import {
  listAlerts,
  listAlertEvents,
  createAlert,
  deleteAlert,
  type AlertRule,
  type AlertEvent,
  type Comparator,
  type Agg,
} from "../api";
import { formatValue } from "../format";
import { useControls } from "../timerange";

const COMPARATORS: { v: Comparator; label: string }[] = [
  { v: "gt", label: ">" },
  { v: "lt", label: "<" },
];
const AGGS: Agg[] = ["avg", "max", "min", "sum", "last"];

const fmtTime = (s: string) => new Date(s).toLocaleString();

function CreateForm({ onCreated }: { onCreated: () => void }) {
  const [name, setName] = useState("");
  const [metric, setMetric] = useState("");
  const [service, setService] = useState("");
  const [comparator, setComparator] = useState<Comparator>("gt");
  const [agg, setAgg] = useState<Agg>("avg");
  const [threshold, setThreshold] = useState("");
  const [windowSecs, setWindowSecs] = useState("300");
  const [error, setError] = useState<string | null>(null);

  const submit = (e: React.FormEvent) => {
    e.preventDefault();
    const value = Number(threshold);
    if (!name.trim() || !metric.trim() || Number.isNaN(value)) {
      setError("name, metric, and a numeric threshold are required");
      return;
    }
    createAlert({
      name: name.trim(),
      metric: metric.trim(),
      service: service.trim() || undefined,
      comparator,
      threshold: value,
      agg,
      window_secs: Number(windowSecs) || 300,
    })
      .then(() => {
        setName("");
        setMetric("");
        setService("");
        setThreshold("");
        setError(null);
        onCreated();
      })
      .catch((err: unknown) => setError(String(err)));
  };

  return (
    <form className="alert-form" onSubmit={submit}>
      <input placeholder="rule name" value={name} onChange={(e) => setName(e.target.value)} />
      <input placeholder="metric" value={metric} onChange={(e) => setMetric(e.target.value)} />
      <input
        placeholder="service (any)"
        value={service}
        onChange={(e) => setService(e.target.value)}
      />
      <select value={agg} onChange={(e) => setAgg(e.target.value as Agg)}>
        {AGGS.map((a) => (
          <option key={a} value={a}>
            {a}
          </option>
        ))}
      </select>
      <select
        value={comparator}
        onChange={(e) => setComparator(e.target.value as Comparator)}
      >
        {COMPARATORS.map((c) => (
          <option key={c.v} value={c.v}>
            {c.label}
          </option>
        ))}
      </select>
      <input
        className="num"
        placeholder="threshold"
        value={threshold}
        onChange={(e) => setThreshold(e.target.value)}
      />
      <label className="muted">
        over{" "}
        <input
          className="num"
          value={windowSecs}
          onChange={(e) => setWindowSecs(e.target.value)}
        />
        s
      </label>
      <button type="submit">add rule</button>
      {error ? <span className="error">{error}</span> : null}
    </form>
  );
}

export default function Alerts() {
  const [rules, setRules] = useState<AlertRule[]>([]);
  const [events, setEvents] = useState<AlertEvent[]>([]);
  const [error, setError] = useState<string | null>(null);
  const { tick } = useControls();

  const reload = () => {
    Promise.all([listAlerts(), listAlertEvents()])
      .then(([r, e]) => {
        setRules(r);
        setEvents(e);
        setError(null);
      })
      .catch((e: unknown) => setError(String(e)));
  };

  // eslint-disable-next-line react-hooks/exhaustive-deps
  useEffect(reload, [tick]);

  const remove = (id: number) => {
    deleteAlert(id)
      .then(reload)
      .catch((e: unknown) => setError(String(e)));
  };

  if (error) return <p className="error">Failed to load: {error}</p>;

  return (
    <div className="alerts">
      <CreateForm onCreated={reload} />

      <h3>Rules</h3>
      {rules.length === 0 ? (
        <p className="muted">No alert rules yet.</p>
      ) : (
        <table>
          <thead>
            <tr>
              <th>state</th>
              <th>name</th>
              <th>condition</th>
              <th>window</th>
              <th></th>
            </tr>
          </thead>
          <tbody>
            {rules.map((r) => (
              <tr key={r.id}>
                <td className={r.firing ? "sev-error" : "muted"}>
                  {r.firing ? "FIRING" : "ok"}
                </td>
                <td>{r.name}</td>
                <td className="mono">
                  {r.agg}({r.metric}
                  {r.service ? `, ${r.service}` : ""}) {r.comparator === "gt" ? ">" : "<"}{" "}
                  {r.threshold}
                </td>
                <td className="muted">{r.window_secs}s</td>
                <td>
                  <button className="back" onClick={() => remove(r.id)}>
                    delete
                  </button>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      )}

      <h3>Recent events</h3>
      {events.length === 0 ? (
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
