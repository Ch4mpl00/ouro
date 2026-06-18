import type { NodeJudgement } from "./schema";

// Judge-noise calibration: how much does the SAME judge wobble when it re-scores
// the SAME node, unchanged, K times? That wobble (σ_judge) is the floor under
// the improver's gate — a patch's Δ on an axis is only believable when it clears
// the judge's own repeatability noise. Noise is MODEL-specific (a stronger judge
// is more self-consistent), so we measure it per (model, prompt version) and
// commit the baseline the gate reads.
//
// This module is pure: it turns repeated verdicts of the same nodes into a
// per-axis noise report. The IO (assembling nodes, calling the backend K times,
// writing the baseline file) lives in scripts/judge-noise.ts.

export const NOISE_AXES = [
  "query_formulation",
  "process",
  "coverage",
  "composition",
  "faithfulness",
] as const;
export type NoiseAxis = (typeof NOISE_AXES)[number];

// The five axis values one verdict carries — numeric when the axis applied and
// was scored, null when the rubric didn't emit it or marked it n/a. Mirrors
// langfuse-scores.ts axisScores (kept local so this module stays dependency-light).
export function extractAxisScores(verdict: NodeJudgement): Record<NoiseAxis, number | null> {
  const card = verdict.scorecard;
  const axis = (name: NoiseAxis): number | null => {
    const a = card.axes.find((x) => x.axis === name);
    return a && a.applicable && a.score !== null ? a.score : null;
  };
  const faith = verdict.faithfulness;
  return {
    query_formulation: axis("query_formulation"),
    process: axis("process"),
    coverage: axis("coverage"),
    composition: axis("composition"),
    faithfulness: faith && faith.applicable && faith.score !== null ? faith.score : null,
  };
}

// Three-state view of ONE axis in ONE verdict, the distinction the noise
// collector needs: a number (scored), "na" (the rubric OWNS this axis but
// returned n/a — a flip if it's numeric elsewhere), or "absent" (the rubric
// doesn't emit this axis at all, e.g. coverage on a planner node — ignore it).
type AxisObservation = number | "na" | "absent";

function classifyAxis(verdict: NodeJudgement, axis: NoiseAxis): AxisObservation {
  if (axis === "faithfulness") {
    const f = verdict.faithfulness;
    if (f === null) return "absent";
    return f.applicable && f.score !== null ? f.score : "na";
  }
  const a = verdict.scorecard.axes.find((x) => x.axis === axis);
  if (a === undefined) return "absent";
  return a.applicable && a.score !== null ? a.score : "na";
}

// One axis observed across the K repeats of a single node: the numeric samples
// (n/a repeats excluded) plus how many repeats were n/a — a flip between numeric
// and n/a is itself instability the gate must know about.
export interface AxisSamples {
  axis: NoiseAxis;
  scores: number[];
  naCount: number;
}

export interface NodeNoiseMeta {
  observationId: string;
  label: string;
  kind: string;
  skill: string;
}

export interface NodeNoise extends NodeNoiseMeta {
  runs: number;
  axes: AxisSamples[];
}

// Fold K verdicts of the SAME node into per-axis samples.
export function collectNodeNoise(meta: NodeNoiseMeta, verdicts: NodeJudgement[]): NodeNoise {
  const perAxis = new Map<NoiseAxis, AxisSamples>(
    NOISE_AXES.map((a) => [a, { axis: a, scores: [], naCount: 0 }]),
  );
  for (const verdict of verdicts) {
    for (const a of NOISE_AXES) {
      const bucket = perAxis.get(a)!;
      const v = classifyAxis(verdict, a);
      if (v === "absent") continue; // rubric doesn't own this axis — not noise
      if (v === "na") bucket.naCount += 1;
      else bucket.scores.push(v);
    }
  }
  return {
    ...meta,
    runs: verdicts.length,
    // Drop axes that never applied to this node (no numeric sample, no n/a noise
    // to report) so the per-node breakdown only shows axes the node owns.
    axes: [...perAxis.values()].filter((s) => s.scores.length > 0 || s.naCount > 0),
  };
}

// ─── aggregate stats ─────────────────────────────────────────────────

function mean(xs: number[]): number {
  return xs.reduce((a, b) => a + b, 0) / xs.length;
}

// Sample stddev (n-1). 0 for fewer than two samples (no spread observable).
export function sampleStdDev(xs: number[]): number {
  if (xs.length < 2) return 0;
  const m = mean(xs);
  const ss = xs.reduce((a, b) => a + (b - m) ** 2, 0);
  return Math.sqrt(ss / (xs.length - 1));
}

// Linear-interpolated percentile over an unsorted array.
export function percentile(xs: number[], p: number): number {
  if (xs.length === 0) return 0;
  const sorted = [...xs].sort((a, b) => a - b);
  if (sorted.length === 1) return sorted[0]!;
  const idx = (p / 100) * (sorted.length - 1);
  const lo = Math.floor(idx);
  const hi = Math.ceil(idx);
  if (lo === hi) return sorted[lo]!;
  return sorted[lo]! * (hi - idx) + sorted[hi]! * (idx - lo);
}

export interface AxisNoise {
  axis: NoiseAxis;
  // Nodes with ≥2 numeric samples (where a stddev is computable).
  nodes: number;
  // sqrt(mean of per-node sample variance) — the pooled repeatability σ, the
  // headline number the gate uses as the noise floor for this axis.
  pooledSigma: number;
  meanSigma: number;
  p90Sigma: number;
  maxSigma: number;
  // Mean score across all numeric samples — context for whether the axis even
  // exercises its range (a near-constant axis has tiny σ for the wrong reason).
  meanScore: number;
  // Nodes where the axis was numeric in some repeats and n/a in others — a
  // distinct, harder-to-gate instability than a wobbling number.
  applicabilityFlips: number;
}

export interface NoiseReport {
  model: string;
  provider: string;
  promptVersion: string;
  runs: number;
  sampleNodes: number;
  totalJudgeCalls: number;
  axes: AxisNoise[];
}

function round(x: number, places = 4): number {
  const f = 10 ** places;
  return Math.round(x * f) / f;
}

// Roll per-node samples up into one noise number per axis.
export function summarizeNoise(
  ctx: { model: string; provider: string; promptVersion: string; runs: number },
  nodes: NodeNoise[],
): NoiseReport {
  const axes: AxisNoise[] = [];
  for (const axis of NOISE_AXES) {
    const samplesPerNode = nodes
      .map((n) => n.axes.find((a) => a.axis === axis))
      .filter((a): a is AxisSamples => a !== undefined);

    const withSpread = samplesPerNode.filter((a) => a.scores.length >= 2);
    if (withSpread.length === 0 && samplesPerNode.every((a) => a.scores.length === 0)) {
      continue; // axis never numerically scored across the corpus — omit it
    }

    const sigmas = withSpread.map((a) => sampleStdDev(a.scores));
    const variances = sigmas.map((s) => s ** 2);
    const allScores = samplesPerNode.flatMap((a) => a.scores);
    const flips = samplesPerNode.filter((a) => a.scores.length > 0 && a.naCount > 0).length;

    axes.push({
      axis,
      nodes: withSpread.length,
      pooledSigma: variances.length ? round(Math.sqrt(mean(variances))) : 0,
      meanSigma: sigmas.length ? round(mean(sigmas)) : 0,
      p90Sigma: round(percentile(sigmas, 90)),
      maxSigma: sigmas.length ? round(Math.max(...sigmas)) : 0,
      meanScore: allScores.length ? round(mean(allScores), 3) : 0,
      applicabilityFlips: flips,
    });
  }

  return {
    model: ctx.model,
    provider: ctx.provider,
    promptVersion: ctx.promptVersion,
    runs: ctx.runs,
    sampleNodes: nodes.length,
    totalJudgeCalls: nodes.reduce((sum, n) => sum + n.runs, 0),
    axes,
  };
}
