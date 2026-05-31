import { createContext, useContext, useEffect, useState, type ReactNode } from "react";

// Relative ranges. `ms: 0` means unbounded ("all").
export const RANGES: { key: string; label: string; ms: number }[] = [
  { key: "15m", label: "15m", ms: 15 * 60_000 },
  { key: "1h", label: "1h", ms: 60 * 60_000 },
  { key: "6h", label: "6h", ms: 6 * 60 * 60_000 },
  { key: "24h", label: "24h", ms: 24 * 60 * 60_000 },
  { key: "7d", label: "7d", ms: 7 * 24 * 60 * 60_000 },
  { key: "all", label: "all", ms: 0 },
];

// Live-refresh intervals. `secs: 0` means off.
export const INTERVALS: { key: string; label: string; secs: number }[] = [
  { key: "off", label: "off", secs: 0 },
  { key: "5s", label: "5s", secs: 5 },
  { key: "15s", label: "15s", secs: 15 },
  { key: "30s", label: "30s", secs: 30 },
  { key: "60s", label: "1m", secs: 60 },
];

interface Controls {
  rangeKey: string;
  setRangeKey: (k: string) => void;
  intervalKey: string;
  setIntervalKey: (k: string) => void;
  /// Bumped on every poll tick and manual refresh; views include it in their
  /// fetch deps so they reload.
  tick: number;
  refresh: () => void;
}

const Ctx = createContext<Controls | null>(null);

export function TimeRangeProvider({ children }: { children: ReactNode }) {
  const [rangeKey, setRangeKey] = useState("1h");
  const [intervalKey, setIntervalKey] = useState("off");
  const [tick, setTick] = useState(0);
  const refresh = () => setTick((t) => t + 1);

  useEffect(() => {
    const secs = INTERVALS.find((i) => i.key === intervalKey)?.secs ?? 0;
    if (secs <= 0) return;
    const id = setInterval(() => setTick((t) => t + 1), secs * 1000);
    return () => clearInterval(id);
  }, [intervalKey]);

  return (
    <Ctx.Provider
      value={{ rangeKey, setRangeKey, intervalKey, setIntervalKey, tick, refresh }}
    >
      {children}
    </Ctx.Provider>
  );
}

export function useControls() {
  const ctx = useContext(Ctx);
  if (!ctx) throw new Error("useControls must be used within a TimeRangeProvider");
  return ctx;
}

// from/to query params for the current range, computed fresh so "now" is current.
export function rangeParams(rangeKey: string): { from?: string } {
  const r = RANGES.find((x) => x.key === rangeKey);
  if (!r || r.ms === 0) return {}; // unbounded
  return { from: new Date(Date.now() - r.ms).toISOString() };
}
