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
      onSelect={(m) =>
        navigate(
          `/metrics/${encodeURIComponent(m.name)}` +
            (m.service ? `?service=${encodeURIComponent(m.service)}` : ""),
        )
      }
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
      onBack={() => navigate("/metrics")}
    />
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
