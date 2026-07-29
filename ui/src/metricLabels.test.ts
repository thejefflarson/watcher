import { describe, expect, it } from "vitest";
import { facetLabels } from "./metricLabels";

// Node/host metrics carry both the reporting agent's identity and the
// node/container they describe. Series must be labeled by the node/container, not
// the agent, and uid/id-style keys stay suppressed.
describe("facetLabels", () => {
  it("labels a single series as 'all'", () => {
    expect(facetLabels([{ attrs: { "k8s.node.name": "node-0" } }])).toEqual(["all"]);
  });

  it("leads the label with the node key ahead of a varying agent key", () => {
    const series = [
      { attrs: { "otelcol.agent": "agent-a", "k8s.node.name": "node-0" } },
      { attrs: { "otelcol.agent": "agent-b", "k8s.node.name": "node-1" } },
    ];
    // Both keys vary (and the agent key is discovered first), but the node identity
    // must lead the label rather than be pushed behind the agent.
    expect(facetLabels(series)).toEqual([
      "node.name=node-0 · otelcol.agent=agent-a",
      "node.name=node-1 · otelcol.agent=agent-b",
    ]);
  });

  it("suppresses uid/id keys and labels by container/node identity", () => {
    const series = [
      {
        attrs: {
          "k8s.node.name": "node-0",
          "k8s.container.name": "kube-proxy",
          "container.id": "aaaa",
          "k8s.pod.uid": "1111",
        },
      },
      {
        attrs: {
          "k8s.node.name": "node-1",
          "k8s.container.name": "coredns",
          "container.id": "bbbb",
          "k8s.pod.uid": "2222",
        },
      },
    ];
    const labels = facetLabels(series);
    expect(labels[0]).toContain("node.name=node-0");
    expect(labels[0]).toContain("container.name=kube-proxy");
    expect(labels[0]).not.toContain("container.id");
    expect(labels[0]).not.toContain("pod.uid");
  });

  it("falls back to varying keys when no identity key is present", () => {
    const series = [
      { attrs: { cpu: "0", state: "idle" } },
      { attrs: { cpu: "1", state: "idle" } },
    ];
    // `state` is constant, so only `cpu` varies and drives the label.
    expect(facetLabels(series)).toEqual(["cpu=0", "cpu=1"]);
  });
});
