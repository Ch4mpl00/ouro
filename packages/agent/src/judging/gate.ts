import { judgeNode } from "./node-judge";
import { extractAxisScores, NOISE_AXES, type NoiseAxis } from "./noise";
import { patchMessages, type ChatMessage } from "./patch";
import type { JudgeBackend } from "./judge-backend";
import type { NodeKind } from "./schema";

// The improver gate: replay a node under a CANDIDATE patch over its FROZEN
// recorded input, re-judge the result, and decide whether the patch moved the
// target axis BEYOND the judge's own noise (σ_judge, measured by judge:noise).
//
// before = the recorded output judged `samples` times (the judge-noise envelope
//          around the node's existing score).
// after  = `samples` FRESH generations from the patched prompt, each judged once
//          (generation + judge variance under the patch).
// The judge yardstick stays fixed: it scores against the ORIGINAL contract, not
// the patched one — a patch nudges the GENERATOR, it must not move the goalposts.

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

// Pure verdict for one axis. A patch's Δ counts only when it clears k·σ_judge:
// without a σ baseline we can't tell signal from noise, so we abstain.
export function gradeAxis(
  axis: NoiseAxis,
  before: number[],
  after: number[],
  sigma: number | null,
  k: number,
): AxisGrade {
  const beforeMean = mean(before);
  const afterMean = mean(after);
  const base: AxisGrade = {
    axis,
    beforeN: before.length,
    beforeMean: beforeMean === null ? null : round(beforeMean),
    afterN: after.length,
    afterMean: afterMean === null ? null : round(afterMean),
    delta: null,
    sigma,
    threshold: null,
    verdict: "n/a",
  };
  if (beforeMean === null || afterMean === null) return base;
  const delta = round(afterMean - beforeMean);
  base.delta = delta;
  if (sigma === null) return { ...base, verdict: "no-baseline" };
  const threshold = round(k * sigma);
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
  // F — the recorded output (the "before" candidate).
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
// shares the user's ChatGPT quota.
export async function runNodeGate(
  deps: GateDeps,
  target: GateNodeTarget,
  patch: string,
  samples: number,
  sigmaByAxis: Partial<Record<NoiseAxis, number>>,
  k: number,
): Promise<NodeGateResult> {
  // before: judge the recorded output `samples` times (judge-noise envelope).
  const before = await judgeOutputs(deps, target, Array.from({ length: samples }, () => target.originalOutput));

  // after: generate `samples` fresh outputs from the patched prompt, judge each.
  const patchedMessages = patchMessages(target.recordedInput, patch);
  const patchedOutputs: string[] = [];
  for (let i = 0; i < samples; i++) {
    patchedOutputs.push(await deps.runModel(patchedMessages, target.model, target.jsonMode));
  }
  const after = await judgeOutputs(deps, target, patchedOutputs);

  const grades = NOISE_AXES.map((axis) =>
    gradeAxis(axis, before[axis], after[axis], sigmaByAxis[axis] ?? null, k),
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
