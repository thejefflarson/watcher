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
