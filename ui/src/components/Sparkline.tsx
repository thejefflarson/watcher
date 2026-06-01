// Inline sparkline — a small multiple drawn with no axes or chrome. Values are
// newest-first (matching the API's `spark` field); we draw oldest→newest.
export default function Sparkline({
  values,
  width = 90,
  height = 18,
}: {
  values: number[];
  width?: number;
  height?: number;
}) {
  if (values.length < 2) return <span className="muted">—</span>;
  const lo = Math.min(...values);
  const hi = Math.max(...values);
  const span = hi - lo || 1;
  const pts = values
    .slice()
    .reverse()
    .map((v, i) => {
      const x = (i / (values.length - 1)) * (width - 2) + 1;
      const y = height - 1 - ((v - lo) / span) * (height - 2);
      return `${x.toFixed(1)},${y.toFixed(1)}`;
    })
    .join(" ");
  return (
    <svg className="spark" width={width} height={height} aria-hidden="true">
      <polyline points={pts} fill="none" stroke="#444" strokeWidth="1" />
    </svg>
  );
}

// A tiny bar chart — for per-interval volume (counter rate, left→right = oldest→
// newest) or a distribution (histogram bucket counts, left→right = low→high).
export function Bars({
  values,
  width = 90,
  height = 18,
}: {
  values: number[];
  width?: number;
  height?: number;
}) {
  if (values.length < 1) return <span className="muted">—</span>;
  const max = Math.max(...values, 1);
  const bw = width / values.length;
  return (
    <svg className="spark" width={width} height={height} aria-hidden="true">
      {values.map((v, i) => {
        const h = Math.max(0, (v / max) * (height - 1));
        return (
          <rect
            key={i}
            x={(i * bw).toFixed(1)}
            y={(height - h).toFixed(1)}
            width={Math.max(1, bw - 0.5).toFixed(1)}
            height={h.toFixed(1)}
            fill="#777"
          />
        );
      })}
    </svg>
  );
}
