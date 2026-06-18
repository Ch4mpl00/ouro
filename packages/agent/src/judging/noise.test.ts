import { describe, expect, it } from "vitest";
import {
  collectNodeNoise,
  extractAxisScores,
  percentile,
  sampleStdDev,
  summarizeNoise,
  type NodeNoise,
} from "./noise";
import type { NodeJudgement } from "./schema";

// Build a planner verdict with given process / query_formulation values
// (null = the axis was n/a or absent).
function plannerVerdict(opts: { process?: number | null; qf?: number | null }): NodeJudgement {
  const axes: NodeJudgement["scorecard"]["axes"] = [];
  const push = (axis: "process" | "query_formulation", v: number | null | undefined) => {
    if (v === undefined) return;
    axes.push({
      axis,
      applicable: v !== null,
      score: v,
      label: v === null ? "n/a" : "ok",
      rationale: "",
      evidence: "",
    });
  };
  push("process", opts.process);
  push("query_formulation", opts.qf);
  return { scorecard: { axes, overall_note: "" }, faithfulness: null };
}

describe("extractAxisScores", () => {
  it("returns the numeric score for applicable axes, null otherwise", () => {
    const v = plannerVerdict({ process: 0.7, qf: null });
    const scores = extractAxisScores(v);
    expect(scores.process).toBe(0.7);
    expect(scores.query_formulation).toBeNull(); // n/a → null
    expect(scores.coverage).toBeNull(); // absent → null
    expect(scores.faithfulness).toBeNull(); // planner has no faithfulness
  });

  it("treats applicable=false / score=null as null even if a number is present", () => {
    const v: NodeJudgement = {
      scorecard: {
        axes: [{ axis: "process", applicable: false, score: 0.9, label: "n/a", rationale: "", evidence: "" }],
        overall_note: "",
      },
      faithfulness: null,
    };
    expect(extractAxisScores(v).process).toBeNull();
  });
});

describe("sampleStdDev / percentile", () => {
  it("is zero for fewer than two samples", () => {
    expect(sampleStdDev([])).toBe(0);
    expect(sampleStdDev([0.5])).toBe(0);
  });

  it("computes the n-1 sample stddev", () => {
    // mean 0.5, deviations ±0.1 → var = (0.01+0.01)/1 = 0.02 → σ ≈ 0.1414
    expect(sampleStdDev([0.4, 0.6])).toBeCloseTo(0.141421, 5);
  });

  it("interpolates percentiles", () => {
    expect(percentile([0, 0.1, 0.2, 0.3], 90)).toBeCloseTo(0.27, 5);
    expect(percentile([0.05], 90)).toBe(0.05);
  });
});

describe("collectNodeNoise", () => {
  it("buckets numeric samples and counts n/a flips per axis", () => {
    const node = collectNodeNoise(
      { observationId: "obs1", label: "attempt-1", kind: "planner", skill: "planner" },
      [
        plannerVerdict({ process: 0.6, qf: 0.8 }),
        plannerVerdict({ process: 0.7, qf: null }), // qf flips to n/a
        plannerVerdict({ process: 0.65, qf: 0.8 }),
      ],
    );
    expect(node.runs).toBe(3);
    const proc = node.axes.find((a) => a.axis === "process")!;
    expect(proc.scores).toEqual([0.6, 0.7, 0.65]);
    expect(proc.naCount).toBe(0);
    const qf = node.axes.find((a) => a.axis === "query_formulation")!;
    expect(qf.scores).toEqual([0.8, 0.8]);
    expect(qf.naCount).toBe(1);
    // Axes the node never touched are omitted from the breakdown.
    expect(node.axes.find((a) => a.axis === "coverage")).toBeUndefined();
  });
});

describe("summarizeNoise", () => {
  it("rolls per-node sigmas into a pooled per-axis noise floor", () => {
    const nodes: NodeNoise[] = [
      collectNodeNoise(
        { observationId: "o1", label: "a", kind: "planner", skill: "planner" },
        [plannerVerdict({ process: 0.4 }), plannerVerdict({ process: 0.6 })], // σ ≈ 0.1414
      ),
      collectNodeNoise(
        { observationId: "o2", label: "b", kind: "planner", skill: "planner" },
        [plannerVerdict({ process: 0.5 }), plannerVerdict({ process: 0.5 })], // σ = 0
      ),
    ];
    const report = summarizeNoise(
      { model: "gpt-5.4", provider: "openai", promptVersion: "n3", runs: 2 },
      nodes,
    );
    expect(report.sampleNodes).toBe(2);
    expect(report.totalJudgeCalls).toBe(4);
    const proc = report.axes.find((a) => a.axis === "process")!;
    expect(proc.nodes).toBe(2);
    // pooled = sqrt(mean(0.02, 0)) = sqrt(0.01) = 0.1
    expect(proc.pooledSigma).toBeCloseTo(0.1, 4);
    expect(proc.maxSigma).toBeCloseTo(0.1414, 3);
    expect(proc.meanScore).toBeCloseTo(0.5, 3);
    expect(proc.applicabilityFlips).toBe(0);
  });

  it("flags applicability flips and omits never-scored axes", () => {
    const nodes: NodeNoise[] = [
      collectNodeNoise(
        { observationId: "o1", label: "a", kind: "planner", skill: "planner" },
        [plannerVerdict({ process: 0.6, qf: 0.8 }), plannerVerdict({ process: 0.6, qf: null })],
      ),
    ];
    const report = summarizeNoise(
      { model: "m", provider: "openai", promptVersion: "n3", runs: 2 },
      nodes,
    );
    const qf = report.axes.find((a) => a.axis === "query_formulation")!;
    expect(qf.applicabilityFlips).toBe(1);
    // coverage/composition/faithfulness never scored → omitted
    expect(report.axes.map((a) => a.axis).sort()).toEqual(["process", "query_formulation"]);
  });
});
