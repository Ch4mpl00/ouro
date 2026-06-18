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

// ─── candidate selection (absolute + σ, never a percentile) ──────────

function axisScore(r: JudgementRecord, axis: NoiseAxis): number | null {
  return r.scores[axis];
}

function nodeId(r: JudgementRecord): string {
  return `${r.traceId}:${r.observationId}`;
}

export interface CandidateSelection {
  // All RECENT-window failures on the axis — the raw material the taxonomy
  // groups into failure modes (the patch target is ONE mode, not all of these).
  candidates: JudgementRecord[];
  // All-time high scorers — the "don't break what works" guardrail. Height, not
  // recency, is what makes a gold standard, so this ignores the window.
  holdout: JudgementRecord[];
}

export interface SelectOpts {
  holdoutSize: number;
  // A node is a candidate failure iff score < absMax (confidently low in
  // absolute terms — so the loop CONVERGES and shuts off on a uniformly-good
  // skill) AND score < bar − k·σ (confidently below a meaningful band edge, not
  // just judge noise on a tight distribution). bar defaults to the "ok" anchor.
  absMax: number;
  bar: number;
  k: number;
  // The axis's judge-noise σ. Null → the σ term is skipped (absolute-only).
  sigma: number | null;
  // Gold-standard threshold for the holdout (all-time score ≥ this).
  holdoutMin: number;
  // ISO cut: candidates must have started at/after this (recent window). Null →
  // no window (consider the whole corpus — used in tests / tiny corpora).
  recentSince: string | null;
}

// Split the corpus into RECENT low-score candidates (the taxonomy's input) and
// an all-time high-score holdout (regression guard), by one axis. Nodes where
// the axis is null (the rubric didn't emit it / marked n/a) carry no signal and
// are ignored. Absolute + σ, never a percentile: a percentile never converges
// (there's always a bottom X%), ignores σ, and is noisy at our small N.
export function selectCandidates(
  records: JudgementRecord[],
  axis: NoiseAxis,
  opts: SelectOpts,
): CandidateSelection {
  const scored = records
    .map((r) => ({ r, s: axisScore(r, axis) }))
    .filter((x): x is { r: JudgementRecord; s: number } => x.s !== null);

  const sigmaFloor = opts.sigma === null ? Infinity : opts.bar - opts.k * opts.sigma;
  const candidates = scored
    .filter((x) => x.s < opts.absMax && x.s < sigmaFloor)
    .filter((x) => opts.recentSince === null || x.r.startedAt >= opts.recentSince)
    .sort((a, b) => a.s - b.s)
    .map((x) => x.r);

  const candidateIds = new Set(candidates.map(nodeId));
  const holdout = scored
    .filter((x) => x.s >= opts.holdoutMin && !candidateIds.has(nodeId(x.r)))
    .sort((a, b) => b.s - a.s)
    .slice(0, opts.holdoutSize)
    .map((x) => x.r);

  return { candidates, holdout };
}

// ─── failure-mode taxonomy (open-coding, improve-time) ───────────────

// A patch fixes a recurring failure MODE, not a number. The lowest-N nodes may
// fail for unrelated reasons (one patch can't fix them); nodes sharing a judge
// complaint are patchable. So between selection and authoring we induce a
// taxonomy over the candidates' rationales and patch the most frequent mode
// (Pareto). At our N (~40–60 short rationales) one LLM open-coding call beats
// embeddings: human-readable, gives frequencies for free, no threshold tuning.
// (Embedding-then-cluster on distilled phrases is the SCALE-UP path.)
export const FailureModeSchema = z.object({
  name: z.string(),
  // Instance-free atomic failure phrase — also the form a general patch takes.
  description: z.string(),
  nodeIds: z.array(z.string()),
});
export type FailureMode = z.infer<typeof FailureModeSchema>;

export const TaxonomySchema = z.object({ modes: z.array(FailureModeSchema) });
export type Taxonomy = z.infer<typeof TaxonomySchema>;

export const TAXONOMY_RESPONSE_SCHEMA: Record<string, unknown> = {
  type: "object",
  properties: {
    modes: {
      type: "array",
      items: {
        type: "object",
        properties: {
          name: { type: "string" },
          description: { type: "string" },
          nodeIds: { type: "array", items: { type: "string" } },
        },
        required: ["name", "description", "nodeIds"],
        additionalProperties: false,
      },
    },
  },
  required: ["modes"],
  additionalProperties: false,
};

export const TAXONOMY_SYSTEM = `You are doing OPEN-CODING of an AI agent's failures on ONE quality axis. You are given short judge rationales, each for one low-scoring node (identified by an id). Induce a small taxonomy of recurring FAILURE MODES.

Rules:
- 3–7 named modes, fewer if the rationales genuinely share fewer patterns. Do NOT invent modes to pad the list.
- Each mode's description is ONE atomic, instance-FREE failure phrase — strip item ids, names, dates, quotes (those are evidence, not the pattern). It should read like the lesson a patch would teach. Example: "omits a concrete infra/data-architecture item the contract asks to include".
- Assign every node id to EXACTLY ONE mode (its dominant complaint). A rationale mixing two complaints goes to the more severe / more frequent one.
- Group by the UNDERLYING defect, not by the judge's wording — two differently-phrased rationales describing the same defect are the same mode.

Return JSON: { "modes": [ { "name": "<short kebab/Title>", "description": "<atomic instance-free phrase>", "nodeIds": ["<id>", ...] }, ... ] }.`;

export function buildTaxonomyUserPrompt(
  skill: string,
  axis: NoiseAxis,
  items: Array<{ id: string; rationale: string }>,
): string {
  const blocks = items
    .map((it) => `- id=${it.id}: ${truncate(it.rationale || "(no rationale recorded)", 600)}`)
    .join("\n");
  return [
    `SKILL: ${skill}`,
    `AXIS: ${axis}`,
    `${items.length} low-scoring node rationale(s):`,
    blocks,
    `\nInduce the failure-mode taxonomy per your instructions. Assign every id exactly once. Return JSON.`,
  ].join("\n\n");
}

export async function induceTaxonomy(
  backend: JudgeBackend,
  args: { skill: string; axis: NoiseAxis; items: Array<{ id: string; rationale: string }> },
): Promise<Taxonomy> {
  const json = await backend.complete({
    name: "taxonomy",
    system: TAXONOMY_SYSTEM,
    user: buildTaxonomyUserPrompt(args.skill, args.axis, args.items),
    schema: TAXONOMY_RESPONSE_SCHEMA,
  });
  return TaxonomySchema.parse(json);
}

// Pick the Pareto-dominant mode (most members), keeping only ids that are real
// candidates (the LLM can hallucinate an id). Ties broken by first-seen order.
export function dominantMode(taxonomy: Taxonomy, validIds: Set<string>): FailureMode | null {
  const cleaned = taxonomy.modes
    .map((m) => ({ ...m, nodeIds: m.nodeIds.filter((id) => validIds.has(id)) }))
    .filter((m) => m.nodeIds.length > 0);
  if (cleaned.length === 0) return null;
  return cleaned.reduce((best, m) => (m.nodeIds.length > best.nodeIds.length ? m : best));
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
- NON-REDUNDANT — you are shown the patch lessons already appended to this skill. Do NOT restate, rephrase, or partially overlap an existing lesson. If the current failure is already covered by an existing lesson, return an empty lesson (the fix is in place; the lows are something else).
- If the examples reveal NO generalizable, fixable pattern (the lows look like judge noise or one-off input problems), or the pattern is already patched, say so in rationale and return an empty lesson.

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
  failureMode: string | null,
  existingPatch: string | null,
  priorFeedback: string | null,
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
    failureMode ? `TARGET FAILURE MODE (fix THIS recurring pattern): ${failureMode}` : "",
    `SKILL_CONTRACT:\n${contract ?? "(none — prompt-only node owned by the planner)"}`,
    `EXISTING PATCH (already appended — do NOT repeat or overlap these lessons):\n${
      existingPatch && existingPatch.trim().length > 0 ? existingPatch.trim() : "(none yet)"
    }`,
    priorFeedback
      ? `PREVIOUS ATTEMPT FAILED THE GATE — do NOT repeat that angle; try a different, sharper fix:\n${priorFeedback}`
      : "",
    `\n${examples.length} failing example(s) of this mode:\n\n${blocks}`,
    `\nWrite the append-only patch per your instructions. Return JSON.`,
  ]
    .filter((s) => s.length > 0)
    .join("\n\n");
}

export async function authorPatch(
  backend: JudgeBackend,
  args: {
    skill: string;
    axis: NoiseAxis;
    contract: string | null;
    examples: PatchExample[];
    failureMode?: string | null;
    existingPatch?: string | null;
    priorFeedback?: string | null;
  },
): Promise<AuthoredPatch> {
  const json = await backend.complete({
    name: "patch",
    system: PATCH_AUTHOR_SYSTEM,
    user: buildAuthorUserPrompt(
      args.skill,
      args.axis,
      args.contract,
      args.examples,
      args.failureMode ?? null,
      args.existingPatch ?? null,
      args.priorFeedback ?? null,
    ),
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
