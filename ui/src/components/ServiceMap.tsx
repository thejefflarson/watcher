import { useEffect, useState } from "react";
import { useNavigate } from "react-router-dom";
import { getServiceMap, type ServiceMapData } from "../api";
import { rowKeyActivate } from "../a11y";
import { firstRunHint } from "../empty";
import { useControls } from "../timerange";

// Simple circular layout — nodes on a ring, edges as arrowed lines.
export default function ServiceMap() {
  const [data, setData] = useState<ServiceMapData | null>(null);
  const [error, setError] = useState<string | null>(null);
  const { tick } = useControls();
  const navigate = useNavigate();

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
  }, [tick]);

  if (error) return <p className="error">Failed to load: {error}</p>;
  if (!data) return <p className="muted">Loading…</p>;
  if (data.nodes.length === 0)
    return <p className="muted">{firstRunHint("traces", "services")}</p>;

  const totalCalls = data.edges.reduce((n, e) => n + e.calls, 0);

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
      <svg
        width={W}
        height={H}
        role="img"
        aria-label={`Service map — ${data.nodes.length} services, ${data.edges.length} call paths, ${totalCalls} calls`}
      >
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
            <path d="M 0 0 L 10 5 L 0 10 z" fill="#777" />
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
                stroke="#cbcbc0"
                strokeWidth={1}
                markerEnd="url(#arrow)"
              />
              <text x={mx} y={my - 4} fill="#6f6f6f" fontSize={11} textAnchor="middle">
                {e.calls}
              </text>
            </g>
          );
        })}
        {data.nodes.map((n) => {
          const p = pos.get(n)!;
          const open = () => navigate(`/traces?service=${encodeURIComponent(n)}`);
          return (
            <g
              key={n}
              className="map-node"
              role="button"
              tabIndex={0}
              aria-label={`View traces for ${n}`}
              onClick={open}
              onKeyDown={rowKeyActivate(open)}
            >
              <title>{`View traces for ${n}`}</title>
              <circle cx={p.x} cy={p.y} r={4} fill="#111" />
              <text x={p.x} y={p.y + 18} fill="#111" fontSize={12} textAnchor="middle">
                {n}
              </text>
            </g>
          );
        })}
      </svg>
    </div>
  );
}
