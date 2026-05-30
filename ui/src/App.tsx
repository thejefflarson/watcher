import { useState } from "react";
import TraceList from "./components/TraceList";
import TraceWaterfall from "./components/TraceWaterfall";
import LogView from "./components/LogView";
import MetricList from "./components/MetricList";
import ServiceMap from "./components/ServiceMap";
import { getToken, setToken } from "./api";

type Tab = "traces" | "logs" | "metrics" | "map";

const TABS: { id: Tab; label: string }[] = [
  { id: "traces", label: "Traces" },
  { id: "logs", label: "Logs" },
  { id: "metrics", label: "Metrics" },
  { id: "map", label: "Service Map" },
];

export default function App() {
  const [tab, setTab] = useState<Tab>("traces");
  const [trace, setTrace] = useState<string | null>(null);

  const go = (t: Tab) => {
    setTab(t);
    setTrace(null);
  };

  const editToken = () => {
    const t = window.prompt("API token (leave blank if the server has none)", getToken());
    if (t !== null) {
      setToken(t.trim());
      go(tab);
    }
  };

  return (
    <div className="app">
      <header>
        <h1>
          <span className="logo">◉</span> watcher
        </h1>
        <nav>
          {TABS.map((t) => (
            <button
              key={t.id}
              className={tab === t.id ? "active" : ""}
              onClick={() => go(t.id)}
            >
              {t.label}
            </button>
          ))}
        </nav>
        <button className="token" title="Set API token" onClick={editToken}>
          🔑
        </button>
      </header>
      <main>
        {tab === "traces" &&
          (trace === null ? (
            <TraceList onSelect={setTrace} />
          ) : (
            <TraceWaterfall traceId={trace} onBack={() => setTrace(null)} />
          ))}
        {tab === "logs" && <LogView />}
        {tab === "metrics" && <MetricList />}
        {tab === "map" && <ServiceMap />}
      </main>
    </div>
  );
}
