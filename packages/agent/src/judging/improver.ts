import { z } from "zod";
import type { JudgeBackend } from "./judge-backend";
import type { AxisGrade, NodeGateResult } from "./gate";
import type { NoiseAxis } from "./noise";
import type { JudgementRecord } from "../db/trace-store";

// The improver's brain (Phase 3, п2): turn a corpus of judged nodes into a
// candidate append-only patch, then decide whether the gate's measured Δ earns
// shipping it. Pure logic lives here (cluster selection, ship decision, prompt
// building); the IO (reading judgements, replaying through the gate, writing the
// .patch.md) lives in scripts/improve.ts.

// ─── cluster selection ───────────────────────────────────────────────

function axisScore(r: JudgementRecord, axis: NoiseAxis): number | null {
  return r.scores[axis];
}

export interface ClusterSelection {
  // Lowest scorers on the axis — the failures a patch should fix.
  cluster: JudgementRecord[];
  // Highest scorers — the "don't break what works" guardrail the gate re-checks.
  holdout: JudgementRecord[];
}

export interface ClusterOpts {
  clusterSize: number;
  holdoutSize: number;
  // Only treat a node as a failure when its axis score is at or below this.
  lowMax: number;
}

// Split the corpus into a low-score cluster (patch target) and a high-score
// holdout (regression guard), by one axis. Nodes where the axis is null (the
// rubric didn't emit it / marked n/a) are ignored — they carry no signal for it.
export function selectClusters(
  records: JudgementRecord[],
  axis: NoiseAxis,
  opts: ClusterOpts,
): ClusterSelection {
  const scored = records
    .map((r) => ({ r, s: axisScore(r, axis) }))
    .filter((x): x is { r: JudgementRecord; s: number } => x.s !== null)
    .sort((a, b) => a.s - b.s);

  const cluster = scored.filter((x) => x.s <= opts.lowMax).slice(0, opts.clusterSize).map((x) => x.r);
  const clusterIds = new Set(cluster.map((r) => `${r.traceId}:${r.observationId}`));
  const holdout = [...scored]
    .reverse()
    .filter((x) => !clusterIds.has(`${x.r.traceId}:${x.r.observationId}`))
    .slice(0, opts.holdoutSize)
    .map((x) => x.r);

  return { cluster, holdout };
}

// ─── patch author ────────────────────────────────────────────────────

export const AuthoredPatchSchema = z.object({
  lesson: z.string(),
  rationale: z.string(),
});
export type AuthoredPatch = z.infer<typeof AuthoredPatchSchema>;

export const PATCH_RESPONSE_SCHEMA: Record<string, unknown> = {
  type: "object",
  properties: {
    lesson: { type: "string" },
    rationale: { type: "string" },
  },
  required: ["lesson", "rationale"],
  additionalProperties: false,
};

// Kept deliberate and constrained: the author writes ONE appended lesson, never
// a rewrite, never a contradiction of the body. Overfitting to a single example
// is the failure mode to avoid — the lesson must generalize.
export const PATCH_AUTHOR_SYSTEM = `You improve an AI agent's skill by writing ONE short APPEND-ONLY patch: extra instruction text that will be appended AFTER the skill's existing body (which you must NOT rewrite or contradict). You are given the skill's contract, the axis that is scoring low, and concrete failing examples (each with the input the node received, the output it produced, and the judge's rationale for the low score).

Write a lesson that fixes the PATTERN behind these failures on that axis:
- Concrete but GENERAL — a rule the agent can apply to new inputs, not a fix hard-coded to one example. Do not name specific items/dates from the examples.
- Minimal — one or a few markdown bullet points. Token-cheap. No preamble.
- Compatible — it must not contradict or duplicate a rule already in the contract; it sharpens or adds, never overrides.
- On-axis — target the specific failure the judge described; do not drift to other concerns.
- If the examples reveal NO generalizable, fixable pattern (the lows look like judge noise or one-off input problems), say so in rationale and return an empty lesson.

Return JSON: { "lesson": "<markdown bullets, or empty>", "rationale": "<one or two sentences: the pattern you found and why this lesson fixes it>" }.`;

export interface PatchExample {
  inputExcerpt: string;
  output: string;
  judgeRationale: string;
}

function truncate(s: string, n: number): string {
  return s.length <= n ? s : `${s.slice(0, n)}… (+${s.length - n} chars)`;
}

export function buildAuthorUserPrompt(
  skill: string,
  axis: NoiseAxis,
  contract: string | null,
  examples: PatchExample[],
): string {
  const blocks = examples
    .map((e, i) =>
      [
        `### Failing example ${i + 1}`,
        `INPUT (excerpt):\n${truncate(e.inputExcerpt, 2000)}`,
        `OUTPUT:\n${truncate(e.output, 2000)}`,
        `JUDGE (${axis}): ${e.judgeRationale}`,
      ].join("\n\n"),
    )
    .join("\n\n");
  return [
    `SKILL: ${skill}`,
    `LOW-SCORING AXIS: ${axis}`,
    `SKILL_CONTRACT:\n${contract ?? "(none — prompt-only node owned by the planner)"}`,
    `\n${examples.length} failing example(s):\n\n${blocks}`,
    `\nWrite the append-only patch per your instructions. Return JSON.`,
  ].join("\n\n");
}

export async function authorPatch(
  backend: JudgeBackend,
  args: { skill: string; axis: NoiseAxis; contract: string | null; examples: PatchExample[] },
): Promise<AuthoredPatch> {
  const json = await backend.complete({
    name: "patch",
    system: PATCH_AUTHOR_SYSTEM,
    user: buildAuthorUserPrompt(args.skill, args.axis, args.contract, args.examples),
    schema: PATCH_RESPONSE_SCHEMA,
  });
  return AuthoredPatchSchema.parse(json);
}

// ─── ship decision ───────────────────────────────────────────────────

export interface ShipDecision {
  accept: boolean;
  reasons: string[];
  clusterImprove: number;
  clusterRegress: number;
}

function targetGrades(results: NodeGateResult[], axis: NoiseAxis): AxisGrade[] {
  return results.flatMap((n) => n.grades.filter((g) => g.axis === axis));
}

// Accept a patch iff it NET-improves the target axis on the cluster AND causes
// no regression anywhere (cluster collateral OR holdout) — conservative, since
// shipping is automatic. A single regress kills it: prod is the ground truth,
// and a live-trend monitor (п3) is the second line, not this gate.
export function decideShip(
  axis: NoiseAxis,
  cluster: NodeGateResult[],
  holdout: NodeGateResult[],
): ShipDecision {
  const reasons: string[] = [];
  const target = targetGrades(cluster, axis);
  const clusterImprove = target.filter((g) => g.verdict === "improve").length;
  const clusterRegress = target.filter((g) => g.verdict === "regress").length;

  const netImprove = clusterImprove >= 1 && clusterImprove > clusterRegress;
  if (!netImprove) {
    reasons.push(
      `target axis "${axis}" did not net-improve on the cluster (improve=${clusterImprove}, regress=${clusterRegress})`,
    );
  }

  const anyRegress = [...cluster, ...holdout]
    .flatMap((n) => n.grades)
    .some((g) => g.verdict === "regress");
  if (anyRegress) reasons.push("a regression appeared on the cluster or holdout (any axis)");

  const accept = netImprove && !anyRegress;
  if (accept) reasons.push(`accepted: ${axis} improved on ${clusterImprove} cluster node(s), no regressions`);
  return { accept, reasons, clusterImprove, clusterRegress };
}
