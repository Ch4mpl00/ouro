import { judgeNode } from "./node-judge";
import { extractAxisScores, NOISE_AXES, type NoiseAxis } from "./noise";
import { patchMessages, type ChatMessage } from "./patch";
import type { JudgeBackend } from "./judge-backend";
import type { NodeKind } from "./schema";

// The improver gate: replay a node under a CANDIDATE patch over its FROZEN
// recorded input, re-judge the result, and decide whether the patch moved the
// target axis BEYOND the judge's own noise (σ_judge, measured by judge:noise).
//
// before = the STORED judgement score (free — we already judged every node once
//          when building the corpus). A single point, so the accept threshold is
//          widened to k·σ·√(1+1/S) to cover its own judge-noise.
// after  = `samples` FRESH generations from the patched prompt, each judged once
//          (generation + judge variance under the patch).
// The judge yardstick stays fixed: it scores against the ORIGINAL contract, not
// the patched one — a patch nudges the GENERATOR, it must not move the goalposts.
//
// REGENERATION is the irreducible cost: re-judging the OLD output under a patched
// contract is meaningless (the yardstick is the ORIGINAL contract, fixed), so the
// only way to see a patch's effect is to re-run the generator. Everything else —
// re-judging "before" S× — is cut: the corpus already gives us the before.

export type GateVerdict = "improve" | "regress" | "noise" | "no-baseline" | "n/a";

export interface AxisGrade {
  axis: NoiseAxis;
  beforeN: number;
  beforeMean: number | null;
  afterN: number;
  afterMean: number | null;
  delta: number | null;
  sigma: number | null;
  threshold: number | null;
  verdict: GateVerdict;
}

function mean(xs: number[]): number | null {
  return xs.length === 0 ? null : xs.reduce((a, b) => a + b, 0) / xs.length;
}

function round(x: number, places = 4): number {
  const f = 10 ** places;
  return Math.round(x * f) / f;
}

// Pure verdict for one axis. before is the node's single STORED score; after is
// the S fresh patched judgements. A patch's Δ counts only when it clears
// k·σ·√(1+1/S) — the σ envelope around a (single before) vs (S-sample after)
// comparison. Without a σ baseline we can't tell signal from noise, so we abstain.
export function gradeAxis(
  axis: NoiseAxis,
  before: number | null,
  after: number[],
  sigma: number | null,
  k: number,
): AxisGrade {
  const afterMean = mean(after);
  const base: AxisGrade = {
    axis,
    beforeN: before === null ? 0 : 1,
    beforeMean: before === null ? null : round(before),
    afterN: after.length,
    afterMean: afterMean === null ? null : round(afterMean),
    delta: null,
    sigma,
    threshold: null,
    verdict: "n/a",
  };
  if (before === null || afterMean === null) return base;
  const delta = round(afterMean - before);
  base.delta = delta;
  if (sigma === null) return { ...base, verdict: "no-baseline" };
  const threshold = round(k * sigma * Math.sqrt(1 + 1 / after.length));
  base.threshold = threshold;
  if (delta > threshold) return { ...base, verdict: "improve" };
  if (delta < -threshold) return { ...base, verdict: "regress" };
  return { ...base, verdict: "noise" };
}

export interface GateNodeTarget {
  observationId: string;
  kind: NodeKind;
  skill: string;
  label: string;
  contract: string | null;
  // R — what the node received; the judge scores F against this. Unchanged by
  // the patch (the patch only touches the generator's system message).
  inputText: string;
  // F — the recorded output. Kept for display/debug; the "before" score now
  // comes from the stored corpus judgement, not from re-judging this.
  originalOutput: string;
  // Replay material: the recorded chat messages + the model that produced them.
  model: string;
  recordedInput: ChatMessage[];
  jsonMode: boolean;
}

export interface GateDeps {
  backend: JudgeBackend;
  // Re-run the generator under the patched prompt. jsonMode mirrors production
  // (planner emits JSON, composer emits prose).
  runModel: (messages: ChatMessage[], model: string, jsonMode: boolean) => Promise<string>;
}

export interface NodeGateResult {
  node: Pick<GateNodeTarget, "observationId" | "kind" | "skill" | "label">;
  patchedOutputs: string[];
  grades: AxisGrade[];
}

function emptyAxisArrays(): Record<NoiseAxis, number[]> {
  return {
    query_formulation: [],
    process: [],
    coverage: [],
    composition: [],
    faithfulness: [],
  };
}

async function judgeOutputs(
  deps: GateDeps,
  target: GateNodeTarget,
  outputs: string[],
): Promise<Record<NoiseAxis, number[]>> {
  const acc = emptyAxisArrays();
  for (const outputText of outputs) {
    const verdict = await judgeNode(deps.backend, {
      kind: target.kind,
      skill: target.skill,
      contract: target.contract,
      inputText: target.inputText,
      outputText,
    });
    const scores = extractAxisScores(verdict);
    for (const a of NOISE_AXES) {
      const v = scores[a];
      if (v !== null) acc[a].push(v);
    }
  }
  return acc;
}

// Run the gate for ONE node. Sequential by design — codex is the bottleneck and
// shares the user's ChatGPT quota. `storedScores` are the node's existing corpus
// scores (the free "before"); only REGENERATION + after-judging cost codex calls.
export async function runNodeGate(
  deps: GateDeps,
  target: GateNodeTarget,
  patch: string,
  samples: number,
  sigmaByAxis: Partial<Record<NoiseAxis, number>>,
  k: number,
  storedScores: Partial<Record<NoiseAxis, number | null>>,
): Promise<NodeGateResult> {
  // after: generate `samples` fresh outputs from the patched prompt, judge each.
  const patchedMessages = patchMessages(target.recordedInput, patch);
  const patchedOutputs: string[] = [];
  for (let i = 0; i < samples; i++) {
    patchedOutputs.push(await deps.runModel(patchedMessages, target.model, target.jsonMode));
  }
  const after = await judgeOutputs(deps, target, patchedOutputs);

  const grades = NOISE_AXES.map((axis) =>
    gradeAxis(axis, storedScores[axis] ?? null, after[axis], sigmaByAxis[axis] ?? null, k),
  ).filter((g) => g.beforeN > 0 || g.afterN > 0);

  return {
    node: {
      observationId: target.observationId,
      kind: target.kind,
      skill: target.skill,
      label: target.label,
    },
    patchedOutputs,
    grades,
  };
}
