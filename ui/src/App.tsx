import { useEffect, useState } from "react";
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
import Services from "./components/Services";
import Alerts from "./components/Alerts";
import { listServices } from "./api";
import { useControls, rangeParams, RANGES, INTERVALS } from "./timerange";

const TABS: { to: string; label: string }[] = [
  { to: "/traces", label: "Traces" },
  { to: "/logs", label: "Logs" },
  { to: "/metrics", label: "Metrics" },
  { to: "/services", label: "Services" },
  { to: "/map", label: "Service Map" },
  { to: "/alerts", label: "Alerts" },
];

// The global service focus lives in `?service=`; carry it across tab switches and
// drill-in/back navigations so it survives without re-typing.
function serviceSearch(service: string): string {
  return service ? `?service=${encodeURIComponent(service)}` : "";
}

// Route wrappers translate component callbacks into URL navigation, so the
// view (selected trace, drilled-in metric) lives in the address bar.
function TracesRoute() {
  const navigate = useNavigate();
  const { service } = useControls();
  // Keep the focus in the URL through the drill-in so the header + tabs still show it.
  return (
    <TraceList
      onSelect={(id) => navigate(`/traces/${id}${serviceSearch(service)}`)}
    />
  );
}

function TraceRoute() {
  const { traceId } = useParams();
  const navigate = useNavigate();
  const { service } = useControls();
  return (
    <TraceWaterfall
      traceId={traceId!}
      onBack={() => navigate(`/traces${serviceSearch(service)}`)}
    />
  );
}

function MetricsRoute() {
  const navigate = useNavigate();
  return (
    <MetricList
      onSelect={(m) => {
        const qs = new URLSearchParams();
        if (m.service) qs.set("service", m.service);
        if (m.unit) qs.set("unit", m.unit);
        if (m.kind) qs.set("kind", m.kind);
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
      kind={params.get("kind")}
      onBack={() => navigate(`/metrics${serviceSearch(params.get("service") ?? "")}`)}
    />
  );
}

// Global service focus — a header <select> before the range control. Populated
// from the services with data in range; keeps a drilled-in service selectable
// even when it's outside the current window's list (TraceList's old trick).
function ServiceFocus() {
  const { service, setService, rangeKey, tick } = useControls();
  const [services, setServices] = useState<string[]>([]);

  useEffect(() => {
    let active = true;
    listServices(rangeParams(rangeKey))
      .then((rows) => active && setServices(rows.map((r) => r.service).sort()))
      .catch(() => active && setServices([]));
    return () => {
      active = false;
    };
  }, [rangeKey, tick]);

  const options =
    services.includes(service) || !service ? services : [service, ...services];

  return (
    <select
      value={service}
      onChange={(e) => setService(e.target.value)}
      title="Service focus (scopes every tab)"
    >
      <option value="">all services</option>
      {options.map((s) => (
        <option key={s} value={s}>
          {s}
        </option>
      ))}
    </select>
  );
}

function Controls() {
  const { rangeKey, setRangeKey, intervalKey, setIntervalKey, refresh } = useControls();
  return (
    <div className="controls">
      <ServiceFocus />
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
  const { service } = useControls();
  const search = serviceSearch(service);
  return (
    <div className="app">
      <header>
        <h1>watcher</h1>
        <nav>
          {TABS.map((t) => (
            <NavLink
              key={t.to}
              // Carry the service focus across tabs so it isn't dropped on switch.
              to={{ pathname: t.to, search }}
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
          <Route path="/services" element={<Services />} />
          <Route path="/map" element={<ServiceMap />} />
          <Route path="/alerts" element={<Alerts />} />
          <Route path="*" element={<Navigate to="/traces" replace />} />
        </Routes>
      </main>
    </div>
  );
}
