// Human-friendly metric value formatting, driven by the OTel `unit` string.

const BYTE_UNITS = new Set(["by", "byte", "bytes"]);
const BYTE_SCALE = ["B", "KB", "MB", "GB", "TB", "PB", "EB"];

// 1.23 GB — binary (1024) scaling with familiar labels.
export function formatBytes(n: number): string {
  if (!Number.isFinite(n)) return String(n);
  const sign = n < 0 ? "-" : "";
  let v = Math.abs(n);
  let i = 0;
  while (v >= 1024 && i < BYTE_SCALE.length - 1) {
    v /= 1024;
    i++;
  }
  const digits = i === 0 ? 0 : v < 10 ? 2 : v < 100 ? 1 : 0;
  return `${sign}${v.toFixed(digits)} ${BYTE_SCALE[i]}`;
}

const grouped = new Intl.NumberFormat(undefined, { maximumFractionDigits: 2 });
const compact = new Intl.NumberFormat(undefined, {
  notation: "compact",
  maximumFractionDigits: 2,
});

// Format a metric value for display. Bytes get scaled to KB/MB/GB…; other large
// numbers use compact notation (1.2M); the unit is appended unless dimensionless.
export function formatValue(value: number | null | undefined, unit?: string | null): string {
  if (value === null || value === undefined || Number.isNaN(value)) return "—";
  const u = (unit ?? "").trim();
  if (BYTE_UNITS.has(u.toLowerCase())) return formatBytes(value);
  const body = Math.abs(value) >= 100_000 ? compact.format(value) : grouped.format(value);
  return u && u !== "1" ? `${body} ${u}` : body;
}
