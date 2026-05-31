import {
  NavLink,
  Navigate,
  Route,
  Routes,
  useNavigate,
  useParams,
  useSearchParams,
} from "react-router-dom";
import TraceList from "./components/TraceList";
import TraceWaterfall from "./components/TraceWaterfall";
import LogView from "./components/LogView";
import MetricList from "./components/MetricList";
import MetricChart from "./components/MetricChart";
import ServiceMap from "./components/ServiceMap";
import Alerts from "./components/Alerts";
import { useControls, RANGES, INTERVALS } from "./timerange";

const TABS: { to: string; label: string }[] = [
  { to: "/traces", label: "Traces" },
  { to: "/logs", label: "Logs" },
  { to: "/metrics", label: "Metrics" },
  { to: "/map", label: "Service Map" },
  { to: "/alerts", label: "Alerts" },
];

// Route wrappers translate component callbacks into URL navigation, so the
// view (selected trace, drilled-in metric) lives in the address bar.
function TracesRoute() {
  const navigate = useNavigate();
  return <TraceList onSelect={(id) => navigate(`/traces/${id}`)} />;
}

function TraceRoute() {
  const { traceId } = useParams();
  const navigate = useNavigate();
  return <TraceWaterfall traceId={traceId!} onBack={() => navigate("/traces")} />;
}

function MetricsRoute() {
  const navigate = useNavigate();
  return (
    <MetricList
      onSelect={(m) => {
        const qs = new URLSearchParams();
        if (m.service) qs.set("service", m.service);
        if (m.unit) qs.set("unit", m.unit);
        const s = qs.toString();
        navigate(`/metrics/${encodeURIComponent(m.name)}${s ? `?${s}` : ""}`);
      }}
    />
  );
}

function MetricRoute() {
  const { name } = useParams();
  const [params] = useSearchParams();
  const navigate = useNavigate();
  return (
    <MetricChart
      name={name!}
      service={params.get("service")}
      unit={params.get("unit")}
      onBack={() => navigate("/metrics")}
    />
  );
}

function Controls() {
  const { rangeKey, setRangeKey, intervalKey, setIntervalKey, refresh } = useControls();
  return (
    <div className="controls">
      <select
        value={rangeKey}
        onChange={(e) => setRangeKey(e.target.value)}
        title="Time range"
      >
        {RANGES.map((r) => (
          <option key={r.key} value={r.key}>
            {r.label}
          </option>
        ))}
      </select>
      <select
        value={intervalKey}
        onChange={(e) => setIntervalKey(e.target.value)}
        title="Live refresh"
      >
        {INTERVALS.map((i) => (
          <option key={i.key} value={i.key}>
            {i.key === "off" ? "live: off" : `live: ${i.label}`}
          </option>
        ))}
      </select>
      <button className="refresh" onClick={refresh} title="Refresh now">
        ↻
      </button>
    </div>
  );
}

export default function App() {
  return (
    <div className="app">
      <header>
        <h1>watcher</h1>
        <nav>
          {TABS.map((t) => (
            <NavLink
              key={t.to}
              to={t.to}
              className={({ isActive }) => (isActive ? "active" : "")}
            >
              {t.label}
            </NavLink>
          ))}
        </nav>
        <Controls />
      </header>
      <main>
        <Routes>
          <Route path="/" element={<Navigate to="/traces" replace />} />
          <Route path="/traces" element={<TracesRoute />} />
          <Route path="/traces/:traceId" element={<TraceRoute />} />
          <Route path="/logs" element={<LogView />} />
          <Route path="/metrics" element={<MetricsRoute />} />
          <Route path="/metrics/:name" element={<MetricRoute />} />
          <Route path="/map" element={<ServiceMap />} />
          <Route path="/alerts" element={<Alerts />} />
          <Route path="*" element={<Navigate to="/traces" replace />} />
        </Routes>
      </main>
    </div>
  );
}
