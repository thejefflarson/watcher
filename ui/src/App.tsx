import { useState } from "react";
import TraceList from "./components/TraceList";
import TraceWaterfall from "./components/TraceWaterfall";
import LogView from "./components/LogView";

type Tab = "traces" | "logs";

export default function App() {
  const [tab, setTab] = useState<Tab>("traces");
  const [trace, setTrace] = useState<string | null>(null);

  const go = (t: Tab) => {
    setTab(t);
    setTrace(null);
  };

  return (
    <div className="app">
      <header>
        <h1>
          <span className="logo">◉</span> watcher
        </h1>
        <nav>
          <button className={tab === "traces" ? "active" : ""} onClick={() => go("traces")}>
            Traces
          </button>
          <button className={tab === "logs" ? "active" : ""} onClick={() => go("logs")}>
            Logs
          </button>
        </nav>
      </header>
      <main>
        {tab === "traces" &&
          (trace === null ? (
            <TraceList onSelect={setTrace} />
          ) : (
            <TraceWaterfall traceId={trace} onBack={() => setTrace(null)} />
          ))}
        {tab === "logs" && <LogView />}
      </main>
    </div>
  );
}
