import { useEffect, useState } from "react";
import { getServiceMap, type ServiceMapData } from "../api";

// Simple circular layout — nodes on a ring, edges as arrowed lines.
export default function ServiceMap() {
  const [data, setData] = useState<ServiceMapData | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let active = true;
    getServiceMap()
      .then((d) => {
        if (active) {
          setData(d);
          setError(null);
        }
      })
      .catch((e: unknown) => active && setError(String(e)));
    return () => {
      active = false;
    };
  }, []);

  if (error) return <p className="error">Failed to load: {error}</p>;
  if (!data) return <p className="muted">Loading…</p>;
  if (data.nodes.length === 0)
    return <p className="muted">No services yet — send some traces.</p>;

  const W = 720;
  const H = 520;
  const cx = W / 2;
  const cy = H / 2;
  const R = Math.min(W, H) / 2 - 90;

  const pos = new Map<string, { x: number; y: number }>();
  data.nodes.forEach((n, i) => {
    const a = (2 * Math.PI * i) / data.nodes.length - Math.PI / 2;
    pos.set(n, { x: cx + R * Math.cos(a), y: cy + R * Math.sin(a) });
  });

  return (
    <div className="servicemap">
      <svg width={W} height={H} role="img" aria-label="service map">
        <defs>
          <marker
            id="arrow"
            viewBox="0 0 10 10"
            refX="9"
            refY="5"
            markerWidth="7"
            markerHeight="7"
            orient="auto-start-reverse"
          >
            <path d="M 0 0 L 10 5 L 0 10 z" fill="#5b9dff" />
          </marker>
        </defs>
        {data.edges.map((e, i) => {
          const s = pos.get(e.source);
          const t = pos.get(e.target);
          if (!s || !t) return null;
          const mx = (s.x + t.x) / 2;
          const my = (s.y + t.y) / 2;
          return (
            <g key={i}>
              <line
                x1={s.x}
                y1={s.y}
                x2={t.x}
                y2={t.y}
                stroke="#3a4250"
                strokeWidth={1.5}
                markerEnd="url(#arrow)"
              />
              <text x={mx} y={my - 4} fill="#8b94a7" fontSize={11} textAnchor="middle">
                {e.calls}
              </text>
            </g>
          );
        })}
        {data.nodes.map((n) => {
          const p = pos.get(n)!;
          return (
            <g key={n}>
              <circle cx={p.x} cy={p.y} r={26} fill="#171a21" stroke="#5b9dff" strokeWidth={2} />
              <text x={p.x} y={p.y + 42} fill="#d7dce5" fontSize={12} textAnchor="middle">
                {n}
              </text>
            </g>
          );
        })}
      </svg>
    </div>
  );
}
