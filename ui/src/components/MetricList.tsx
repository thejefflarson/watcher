import { useEffect, useState } from "react";
import { listMetrics, type MetricSummary } from "../api";

export default function MetricList() {
  const [metrics, setMetrics] = useState<MetricSummary[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [service, setService] = useState("");

  useEffect(() => {
    let active = true;
    const handle = setTimeout(() => {
      listMetrics({ service: service || undefined })
        .then((m) => {
          if (active) {
            setMetrics(m);
            setError(null);
          }
        })
        .catch((e: unknown) => active && setError(String(e)));
    }, 250);
    return () => {
      active = false;
      clearTimeout(handle);
    };
  }, [service]);

  if (error) return <p className="error">Failed to load: {error}</p>;

  return (
    <div>
      <div className="filters">
        <input
          placeholder="service"
          value={service}
          onChange={(e) => setService(e.target.value)}
        />
      </div>
      {metrics.length === 0 ? (
        <p className="muted">No metrics yet.</p>
      ) : (
        <table>
          <thead>
            <tr>
              <th>Metric</th>
              <th>Service</th>
              <th>Kind</th>
              <th>Unit</th>
              <th className="num">Last value</th>
              <th className="num">Points</th>
              <th>Last seen</th>
            </tr>
          </thead>
          <tbody>
            {metrics.map((m) => (
              <tr key={m.name}>
                <td className="mono">{m.name}</td>
                <td>{m.service ?? "—"}</td>
                <td>{m.kind ?? "—"}</td>
                <td>{m.unit ?? "—"}</td>
                <td className="num">{m.last_value ?? "—"}</td>
                <td className="num">{m.points}</td>
                <td>{new Date(m.last_time).toLocaleTimeString()}</td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </div>
  );
}
