import { describe, expect, it } from "vitest";
import { decideShip, selectClusters } from "./improver";
import type { AxisGrade, NodeGateResult } from "./gate";
import type { NoiseAxis } from "./noise";
import type { JudgementRecord } from "../db/trace-store";

function rec(id: string, coverage: number | null): JudgementRecord {
  return {
    traceId: `t-${id}`,
    observationId: `o-${id}`,
    nodeKind: "compose",
    skill: "news-digest",
    scores: {
      query_formulation: null,
      process: null,
      coverage,
      composition: null,
      faithfulness: null,
    },
    detail: {},
  };
}

describe("selectClusters", () => {
  it("clusters the lowest scorers ≤ lowMax and holds out the highest, no overlap", () => {
    const records = [rec("a", 0.2), rec("b", 0.5), rec("c", 0.9), rec("d", 0.95), rec("e", 0.4)];
    const { cluster, holdout } = selectClusters(records, "coverage", {
      clusterSize: 2,
      holdoutSize: 2,
      lowMax: 0.6,
    });
    expect(cluster.map((r) => r.observationId)).toEqual(["o-a", "o-e"]); // 0.2, 0.4
    expect(holdout.map((r) => r.observationId)).toEqual(["o-d", "o-c"]); // 0.95, 0.9
  });

  it("ignores nodes where the axis is null (rubric didn't emit it / n/a)", () => {
    const records = [rec("a", null), rec("b", 0.3), rec("c", null)];
    const { cluster } = selectClusters(records, "coverage", { clusterSize: 5, holdoutSize: 5, lowMax: 0.6 });
    expect(cluster.map((r) => r.observationId)).toEqual(["o-b"]);
  });

  it("returns an empty cluster when nothing is at or below lowMax", () => {
    const records = [rec("a", 0.8), rec("b", 0.9)];
    const { cluster } = selectClusters(records, "coverage", { clusterSize: 3, holdoutSize: 3, lowMax: 0.6 });
    expect(cluster).toEqual([]);
  });
});

// Build a gate result for one node with the given per-axis verdicts.
function gateResult(label: string, verdicts: Partial<Record<NoiseAxis, AxisGrade["verdict"]>>): NodeGateResult {
  const grades: AxisGrade[] = Object.entries(verdicts).map(([axis, verdict]) => ({
    axis: axis as NoiseAxis,
    beforeN: 3,
    beforeMean: 0.5,
    afterN: 3,
    afterMean: 0.7,
    delta: 0.2,
    sigma: 0.05,
    threshold: 0.1,
    verdict: verdict!,
  }));
  return { node: { observationId: label, kind: "compose", skill: "news-digest", label }, patchedOutputs: [], grades };
}

describe("decideShip", () => {
  it("accepts when the target axis improves on the cluster and nothing regresses", () => {
    const cluster = [gateResult("c1", { coverage: "improve" }), gateResult("c2", { coverage: "improve" })];
    const holdout = [gateResult("h1", { coverage: "noise" })];
    const d = decideShip("coverage", cluster, holdout);
    expect(d.accept).toBe(true);
    expect(d.clusterImprove).toBe(2);
  });

  it("rejects when the target axis does not net-improve on the cluster", () => {
    const cluster = [gateResult("c1", { coverage: "noise" }), gateResult("c2", { coverage: "noise" })];
    const d = decideShip("coverage", cluster, []);
    expect(d.accept).toBe(false);
  });

  it("rejects on any regression — even a collateral axis on the cluster", () => {
    const cluster = [gateResult("c1", { coverage: "improve", composition: "regress" })];
    const d = decideShip("coverage", cluster, []);
    expect(d.accept).toBe(false);
  });

  it("rejects on a holdout regression (don't break what works)", () => {
    const cluster = [gateResult("c1", { coverage: "improve" })];
    const holdout = [gateResult("h1", { coverage: "regress" })];
    const d = decideShip("coverage", cluster, holdout);
    expect(d.accept).toBe(false);
  });
});
