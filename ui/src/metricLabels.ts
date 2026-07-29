// Shared labeling for faceted metric series.

// Strip the noisy `k8s.` prefix so labels read pod.name=… not k8s.pod.name=….
export const shortKey = (k: string) => k.replace(/^k8s\./, "");

// Node / host / container identity keys, most-identifying first. A node/host metric
// (e.g. system.cpu.load_average, k8s.container.restarts) carries both the collector
// agent's identity AND the node/container it describes; label by the latter. Without
// this ordering the first ≤3 varying keys could pick an agent key ahead of the node,
// mislabeling every series by the reporting agent instead of the node.
const IDENTITY_PRIORITY = [
  "k8s.node.name",
  "host.name",
  "host.id",
  "k8s.container.name",
  "container.name",
  "k8s.pod.name",
  "k8s.deployment.name",
  "service.instance.id",
];

// Rank a key by how strongly it identifies the node/container: lower is preferred.
const keyRank = (k: string) => {
  const i = IDENTITY_PRIORITY.indexOf(k);
  return i === -1 ? IDENTITY_PRIORITY.length : i;
};

// Label each series by only the attribute keys that vary across the set, preferring
// node/host/container identity keys and dropping uid/id-style keys when a friendlier
// varying key is available.
export function facetLabels(series: { attrs: Record<string, string> }[]): string[] {
  if (series.length <= 1) return series.map(() => "all");
  const keys = new Set<string>();
  series.forEach((s) => Object.keys(s.attrs).forEach((k) => keys.add(k)));
  const varying = [...keys].filter((k) => {
    const vals = new Set(series.map((s) => s.attrs[k] ?? ""));
    return vals.size > 1;
  });
  const friendly = varying.filter((k) => !k.endsWith(".uid") && !k.endsWith("id"));
  // Node/host/container identity first; otherwise keep the discovered order (stable).
  const labelKeys = (friendly.length ? friendly : varying)
    .sort((a, b) => keyRank(a) - keyRank(b))
    .slice(0, 3);
  return series.map(
    (s) => labelKeys.map((k) => `${shortKey(k)}=${s.attrs[k] ?? "∅"}`).join(" · ") || "—",
  );
}
