// Shared labeling for faceted metric series.

// Strip the noisy `k8s.` prefix so labels read pod.name=… not k8s.pod.name=….
export const shortKey = (k: string) => k.replace(/^k8s\./, "");

// Label each series by only the attribute keys that vary across the set,
// dropping uid/id-style keys when a friendlier varying key is available.
export function facetLabels(series: { attrs: Record<string, string> }[]): string[] {
  if (series.length <= 1) return series.map(() => "all");
  const keys = new Set<string>();
  series.forEach((s) => Object.keys(s.attrs).forEach((k) => keys.add(k)));
  const varying = [...keys].filter((k) => {
    const vals = new Set(series.map((s) => s.attrs[k] ?? ""));
    return vals.size > 1;
  });
  const friendly = varying.filter((k) => !k.endsWith(".uid") && !k.endsWith("id"));
  const labelKeys = (friendly.length ? friendly : varying).slice(0, 3);
  return series.map(
    (s) => labelKeys.map((k) => `${shortKey(k)}=${s.attrs[k] ?? "∅"}`).join(" · ") || "—",
  );
}
