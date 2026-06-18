import { describe, expect, it } from "vitest";
import { decideShip, dominantMode, selectCandidates, type Taxonomy } from "./improver";
import type { AxisGrade, NodeGateResult } from "./gate";
import type { NoiseAxis } from "./noise";
import type { JudgementRecord } from "../db/trace-store";

function rec(id: string, coverage: number | null, startedAt = "2026-06-18T00:00:00.000Z"): JudgementRecord {
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
    startedAt,
  };
}

// Defaults mirroring the locked design: candidate iff score < 0.6 AND
// score < 0.75 − k·σ; holdout = all-time score ≥ 0.85.
const OPTS = { holdoutSize: 5, absMax: 0.6, bar: 0.75, k: 2, holdoutMin: 0.85, recentSince: null as string | null };

describe("selectCandidates", () => {
  it("selects absolute+σ failures (asc) and an all-time high holdout (desc), no overlap", () => {
    const records = [rec("a", 0.2), rec("b", 0.5), rec("c", 0.9), rec("d", 0.95), rec("e", 0.4)];
    const { candidates, holdout } = selectCandidates(records, "coverage", { ...OPTS, sigma: 0.05 });
    // sigmaFloor = 0.75 − 0.1 = 0.65; absMax 0.6 dominates → s < 0.6
    expect(candidates.map((r) => r.observationId)).toEqual(["o-a", "o-e", "o-b"]); // 0.2,0.4,0.5
    expect(holdout.map((r) => r.observationId)).toEqual(["o-d", "o-c"]); // 0.95, 0.9
  });

  it("the σ term excludes near-bar lows on a noisy axis (not just judge wobble)", () => {
    const records = [rec("a", 0.4), rec("b", 0.5)];
    // sigma 0.15 → sigmaFloor = 0.75 − 0.3 = 0.45; 0.5 fails absMax but not the σ floor.
    const { candidates } = selectCandidates(records, "coverage", { ...OPTS, sigma: 0.15 });
    expect(candidates.map((r) => r.observationId)).toEqual(["o-a"]);
  });

  it("draws the cluster from the recent window but the holdout from all time", () => {
    const records = [
      rec("old-low", 0.3, "2026-01-01T00:00:00.000Z"),
      rec("new-low", 0.3, "2026-06-17T00:00:00.000Z"),
      rec("old-high", 0.9, "2026-01-01T00:00:00.000Z"),
    ];
    const { candidates, holdout } = selectCandidates(records, "coverage", {
      ...OPTS,
      sigma: 0.05,
      recentSince: "2026-06-01T00:00:00.000Z",
    });
    expect(candidates.map((r) => r.observationId)).toEqual(["o-new-low"]); // old low dropped
    expect(holdout.map((r) => r.observationId)).toEqual(["o-old-high"]); // height, not recency
  });

  it("ignores nodes where the axis is null (rubric didn't emit it / n/a)", () => {
    const records = [rec("a", null), rec("b", 0.3), rec("c", null)];
    const { candidates } = selectCandidates(records, "coverage", { ...OPTS, sigma: 0.05 });
    expect(candidates.map((r) => r.observationId)).toEqual(["o-b"]);
  });

  it("returns no candidates when the floor has reached the ceiling (all good)", () => {
    const records = [rec("a", 0.8), rec("b", 0.9)];
    const { candidates } = selectCandidates(records, "coverage", { ...OPTS, sigma: 0.05 });
    expect(candidates).toEqual([]);
  });
});

describe("dominantMode", () => {
  const taxonomy: Taxonomy = {
    modes: [
      { name: "omits-item", description: "omits a concrete item", nodeIds: ["t-a:o-a", "t-b:o-b"] },
      { name: "off-contract", description: "includes off-contract opinion", nodeIds: ["t-c:o-c"] },
    ],
  };

  it("picks the Pareto-dominant mode (most valid members)", () => {
    const valid = new Set(["t-a:o-a", "t-b:o-b", "t-c:o-c"]);
    expect(dominantMode(taxonomy, valid)?.name).toBe("omits-item");
  });

  it("drops hallucinated ids and re-ranks on the cleaned counts", () => {
    // only one of omits-item's ids is real → off-contract now wins
    const valid = new Set(["t-a:o-a", "t-c:o-c", "t-x:o-x"]);
    const tax: Taxonomy = {
      modes: [
        { name: "omits-item", description: "x", nodeIds: ["t-a:o-a", "t-ghost:o-ghost"] },
        { name: "off-contract", description: "y", nodeIds: ["t-c:o-c", "t-x:o-x"] },
      ],
    };
    expect(dominantMode(tax, valid)?.name).toBe("off-contract");
  });

  it("returns null when no candidate mapped", () => {
    expect(dominantMode({ modes: [{ name: "m", description: "d", nodeIds: ["ghost"] }] }, new Set())).toBeNull();
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
