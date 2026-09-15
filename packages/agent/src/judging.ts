import { readFileSync } from "node:fs";
import OpenAI from "openai";
import type { ChatCompletionMessageParam } from "openai/resources/chat/completions";
import { z } from "zod";
import {
  DEEPSEEK_BASE_URL,
  GEMINI_BASE_URL,
  appendPatch,
  createSkillStore,
  retryOnTransient,
  type SkillStore,
} from "./agent-loop";
import { createCodexClient, type CodexClient } from "./codex-client";
import type { ImproverStore, JudgementRecord, TraceStore } from "./db";
import {
  JUDGE_NODE_META,
  type Observation,
  type TraceRecord,
  type TraceSummary,
} from "./tracing";
import { apiPost, fetchRecentTraces, fetchTraceById } from "./scripts/langfuse-api";

// Evaluation, end to end: the per-node judge that scores a run, the noise
// model that says whether a score difference is real, the gate that decides
// whether a proposed skill patch actually helps, the improver that writes
// those patches, and the two cron workers that drive it all.
//
// Read top to bottom — each section only depends on the ones above it:
//
//   schema           the scorecard: axes, rubric shapes, JUDGE_PROMPT_VERSION
//   patch            the append-only skill-patch overlay
//   monitor          post-ship trend check + auto-revert arithmetic
//   trace source     one interface over Langfuse-fetched and locally
//                    mirrored runs, so everything below is source-agnostic
//   noise            judge variance: repeat a judgement S times, derive the
//                    per-axis sigma a real improvement must clear
//   print            human-readable scorecard rendering for the CLIs
//   sigma baseline   the committed noise baseline, read from
//                    ./judging-noise-baseline.json
//   judge backend    the LLM/codex call that returns one scorecard
//   node judge       score ONE generative observation
//   materials        assemble what the judge sees for a node
//   langfuse scores  write scores back to Langfuse so they show in the UI
//   gate             does a candidate patch beat the baseline beyond noise?
//   gate runtime     replay a node under a candidate patch
//   improver         cluster failures, author a lesson, gate it
//   improve cycle    one (skill, axis) cycle: select → author → gate → ship
//   improve worker   the cron loop over every (skill, axis), with monitoring
//   judge worker     the cron loop that scores fresh runs
//
// What stays outside: ./agent-loop (the skill store and the retry helper),
// ./db (the trace mirror and improver state), ./tracing (the observation
// shapes and the judge-node tag), ./codex-client and ./scripts/langfuse-api
// (transports). The CLIs that drive this live in ./scripts/.

// ═══════════════════════════════════════════════════════════════════
// Schema
// ═══════════════════════════════════════════════════════════════════

export const JUDGE_MODEL = "gpt-5.4";
// Per-node rubrics (n2). Supersedes the whole-run v3 prompt: each generative
// node is scored against its OWNER contract, so the signal pins to one skill.
// n2: planner `process` axis hardened to penalize unresolved referential
// ambiguity (pronouns to unseen context → must gather+replan) and to always
// judge the planner substantively.
// n3: calibration — an unresolved referent DEGRADES (weak/ok), it is not a
// fail when the core task still executes; reserve fail for not-accomplished.
// Bump re-judges the corpus at the new version.
export const JUDGE_PROMPT_VERSION = "n4";

// A judgeable node's owner type. Selects the rubric (planner axes vs composer
// axes) and whether faithfulness applies. `compose` and `agent` share a rubric
// — both produce a final text from a known input.
export type NodeKind = "planner" | "compose" | "agent";

// All axes any rubric can emit. The Zod schema is the lenient PARSE side
// (accepts whatever a rubric returned); the per-rubric RESPONSE_SCHEMA below
// is the strict FORCE side that pins each rubric to exactly its own axes.
export const AxisResultSchema = z.object({
  axis: z.enum(["coverage", "query_formulation", "composition", "process"]),
  applicable: z.boolean(),
  score: z.number().nullable(),
  label: z.enum(["fail", "weak", "ok", "strong", "n/a"]),
  rationale: z.string(),
  evidence: z.string(),
});

export const ScorecardSchema = z.object({
  axes: z.array(AxisResultSchema),
  overall_note: z.string(),
});
export type Scorecard = z.infer<typeof ScorecardSchema>;

// Strict JSON-schema generator — one shape, the axis enum varies per rubric so
// the model can only return the axes that node's owner is responsible for.
function responseSchemaForAxes(axes: string[]): Record<string, unknown> {
  return {
    type: "object",
    properties: {
      axes: {
        type: "array",
        items: {
          type: "object",
          properties: {
            axis: { type: "string", enum: axes },
            applicable: { type: "boolean" },
            score: { type: ["number", "null"] },
            label: { type: "string", enum: ["fail", "weak", "ok", "strong", "n/a"] },
            rationale: { type: "string" },
            evidence: { type: "string" },
          },
          required: ["axis", "applicable", "score", "label", "rationale", "evidence"],
          additionalProperties: false,
        },
      },
      overall_note: { type: "string" },
    },
    required: ["axes", "overall_note"],
    additionalProperties: false,
  };
}

export const PLANNER_RESPONSE_SCHEMA = responseSchemaForAxes(["query_formulation", "process"]);
export const COMPOSER_RESPONSE_SCHEMA = responseSchemaForAxes(["coverage", "composition"]);

export const FaithClaimSchema = z.object({
  claim: z.string(),
  verdict: z.enum(["supported", "partial", "unsupported"]),
  evidence: z.string(),
});

export const FaithfulnessSchema = z.object({
  applicable: z.boolean(),
  claims: z.array(FaithClaimSchema),
  score: z.number().nullable(),
  note: z.string(),
});
export type Faithfulness = z.infer<typeof FaithfulnessSchema>;

export const FAITH_RESPONSE_SCHEMA = {
  type: "object",
  properties: {
    applicable: { type: "boolean" },
    claims: {
      type: "array",
      items: {
        type: "object",
        properties: {
          claim: { type: "string" },
          verdict: { type: "string", enum: ["supported", "partial", "unsupported"] },
          evidence: { type: "string" },
        },
        required: ["claim", "verdict", "evidence"],
        additionalProperties: false,
      },
    },
    score: { type: ["number", "null"] },
    note: { type: "string" },
  },
  required: ["applicable", "claims", "score", "note"],
  additionalProperties: false,
};

// ─── planner node rubric ─────────────────────────────────────────────
// Judges ONE planner generation: the orchestration decision frozen as a
// Workflow JSON plan, against the planner contract over the signal. No
// retrieval replay — the plan IS the output; execution is deterministic code.
export const PLANNER_NODE_PROMPT = `You are a rigorous evaluation judge for the PLANNER (orchestrator) of an AI agent. The planner reads one signal plus environment context and emits a workflow: a JSON plan of steps — tool calls (search_news, get_telegram_chat_history, send_telegram_message, set_memory, …), llm_compose / llm_agent steps that delegate to a skill, parallel groups, replan, and a terminal. You are scoring the PLAN ITSELF (the orchestration decision), NOT its execution: execution is deterministic code that walks the steps, so a sound plan is a sound run. You did not author the plan and have no stake in it.

Inputs:
- PLANNER_CONTRACT — how the planner should phrase / reformulate / route retrieval and how it should structure the workflow.
- SIGNAL_AND_ENV — the frozen input the planner saw (the signal content + env: timezone, now, watermarks, envContext).
- PLAN — the planner's output: the Workflow JSON (the steps to run).

Score EXACTLY these two axes from 0 to 1 (fail < 0.3, weak < 0.5, ok < 0.75, strong >= 0.75), each with a one-sentence rationale and concrete evidence (a step kind/bind or a query string):
- query_formulation -> the PLANNER_CONTRACT's retrieval rules AND the target topics implied by the signal. Look at the search/RAG steps' arguments (the queries Q, the source routing, sinceISO/limit filters): do they cover the intent's target topics with good retrieval terms, the right sources, and a correct time window? Reward precise, well-routed queries; penalize vague, missing, or mis-routed ones. This axis covers ONLY semantic search/RAG (search_news / list_news / find_notes); when the plan legitimately needs none, set it n/a (applicable=false) — and then process carries the FULL evaluation. (Gathering decisions that are NOT semantic search — e.g. fetching chat history — belong to process, not here.)
- process -> the PLANNER_CONTRACT. Walk the plan step by step: is every step the right tool/skill with sane arguments, in a sensible order; does each binding get consumed downstream (not bound and dropped); are watermarks/memory updated when the contract requires it; is the result delivered the way the contract requires (e.g. send to the right chat); is the terminal/replan structure correct? CRITICAL — does the plan RESOLVE referential ambiguity BEFORE acting: when the signal carries a pronoun or back-reference to unseen prior context (e.g. «ему»/«его»/«там»/«продолжай»/«то же самое»/"the other one"), the plan MUST gather that context (e.g. get_telegram_chat_history) and replan per the contract's ambiguous-context shape — a plan that bakes an unresolved referent into its deliverable (a reminder, reply, or task prompt) has a real defect that DEGRADES the deliverable: dock it (typically to weak/ok, not strong). Calibrate by what the plan ACCOMPLISHES: reserve fail (<0.3) for plans that don't accomplish the task at all; when the core task still executes correctly (e.g. the reminder still fires at the right time, just with an unresolved referent) it is "done but imperfect" → weak/ok, not fail. Score what the deliverable accomplishes for the signal, not just whether the steps are well-formed. Redundant, missing, contradictory, or dangling steps lower the score.

Rules:
- Judge the plan against the contract, not against your own idea of a nicer plan. A different-but-valid plan is not a defect.
- If the signal legitimately calls for a tiny plan (e.g. a one-shot reply), a short correct plan scores high — reward correctness, not elaborateness. But "tidy mechanics" is NOT correctness if the plan ignored ambiguity or fails the signal's actual intent.
- The planner is ALWAYS substantively judged. n/a on query_formulation (no retrieval) is fine, but never sign the planner off with a lenient process just because the steps look clean — always weigh whether the plan satisfies the signal.
- Ground every claim in the PLAN / SIGNAL_AND_ENV. Never invent steps or queries that aren't there.`;

// ─── composer / agent node rubric ────────────────────────────────────
// Judges ONE llm_compose generation (or an llm_agent step black-box): the
// final text the node produced, against the node's skill contract and the
// input it actually received. The composer does not orchestrate — it receives
// candidates as input and writes text; never penalize it for tool calls.
export const COMPOSER_NODE_PROMPT = `You are a rigorous evaluation judge for ONE composer node of an AI agent — a single skill that receives gathered candidates (and any chat history) as INPUT and writes a final text (F). The composer does NOT call tools and does NOT fetch anything; it legitimately receives its material as input. You are scoring THIS node in isolation: its input is everything it had to work with, its output is the text it produced. You did not author it and have no stake in it.

Inputs:
- COMPOSER_CONTRACT — the skill: how candidates should be filtered and the output composed (format, thresholds, length). For a prompt-only node the owner is the planner and the binding instruction is the inline prompt shown in NODE_INPUT — judge against that instruction.
- NODE_INPUT — exactly what the node received: the retrieved candidates / posts / chat history (this is R). The system message is the contract above; the user message carries R.
- NODE_OUTPUT — the text the node produced (this is F).

Score EXACTLY these two axes from 0 to 1 (fail < 0.3, weak < 0.5, ok < 0.75, strong >= 0.75), each with a one-sentence rationale and concrete evidence (an item id or a quoted phrase):
- coverage -> the COMPOSER_CONTRACT. Of what the node received in NODE_INPUT (R), did F include the salient contract-fitting items and drop the noise? Missing a clearly contract-fitting item lowers it; padding with off-contract noise lowers it.
- composition -> the COMPOSER_CONTRACT. Does F respect the contract's EXPLICIT structural rules — the format/shape it prescribes and any stated length or threshold limit? Judge OBJECTIVELY against rules the contract actually states: dock ONLY for a clear, citable CONTRADICTION with such a rule (wrong format, a blown length cap, a violated "stop if < N matches"). Do NOT judge tone, voice, style, polish, or elegance; do NOT penalize anything the contract is silent about — silence is not a violation; factual fabrication is the faithfulness axis's job, not this one. Absent a concrete contract violation, score composition strong.

Rules:
- Obey the contract. If it says "< 3 matches -> short message and stop", a short / empty output is CORRECT when R really held < 3 contract-fitting, non-duplicate items — judge whether the count was right, not whether it produced a long digest.
- NEVER penalize the composer for orchestration. That a result is later sent via a Telegram tool, or that history arrived via a fetch tool, is the planner's job and is not even visible in this node's input. A contract line like "do not call any Telegram tool" describes the composer's role (it composes, it doesn't fetch); it is satisfied as long as F itself doesn't try to call tools.
- If an axis does not apply (an empty output has nothing to compose, nothing in R to cover), set applicable=false, score=null, label="n/a".
- Reward neither length nor fluency. A correct short output beats a verbose wrong one.
- Ground every claim in NODE_INPUT / NODE_OUTPUT. Never invent items that aren't in R.`;

// Faithfulness sub-judge — claim decomposition for a compose/agent node. R is
// the NODE's own input (not the whole run): a claim is grounded iff it appears
// in the snippets this node received.
export const FAITH_SYSTEM_PROMPT = `You are a faithfulness checker for ONE composer node of an AI agent. You verify that every factual claim in the node's final text (F = NODE_OUTPUT) is grounded in the material the node received (R = NODE_INPUT: the retrieved snippets and any chat history). Quoting specifics — numbers, counts, dates, version numbers — not present in R is the single most damaging error class.

Method:
1. Extract ATOMIC factual claims from F. Focus on verifiable specifics: numbers/counts, dates, named entities, concrete events, and comparisons ("up from 63 the day before").
2. For each claim, look for support in R. Verdict:
   - supported — the claim and its specifics appear in some item in R.
   - partial — the gist is backed but a specific (number/date/name) is missing, altered, or aggregated beyond what any single item states.
   - unsupported — no item in R backs it; likely fabricated or editorialized.
3. Cite the supporting (or contradicting) item id as evidence, or "none".

Rules:
- Judge only F's factual content. The text's own header/date line, category labels, and emojis are not claims.
- A hard number synthesized by aggregating several items is at best PARTIAL unless an item states that number.
- If F is empty / a "тихий день" style message with no factual claims, set applicable=false, score=null, claims=[].
- score = (count(supported) + 0.5 * count(partial)) / total_claims, rounded to 2 decimals.
- Ground every verdict in R; never invent item content.`;

// Common rendering of one node's input/output blocks, shared by all three
// user prompts so the judge sees the same evidence framing each time.
function nodeBlocks(inputLabel: string, inputText: string, outputLabel: string, outputText: string): string {
  return `<${inputLabel}>
${inputText || "(empty)"}
</${inputLabel}>

<${outputLabel}>
${outputText || "(empty)"}
</${outputLabel}>`;
}

export function buildPlannerUserPrompt(
  contract: string | null,
  nodeInput: string,
  nodeOutput: string,
): string {
  return `<planner_contract skill="planner">
${contract ?? "(planner contract unavailable)"}
</planner_contract>

${nodeBlocks("signal_and_env", nodeInput, "plan", nodeOutput)}

Score this plan. Return JSON matching the schema, with exactly these two axes: query_formulation, process.`;
}

export function buildComposerUserPrompt(
  skill: string,
  contract: string | null,
  nodeInput: string,
  nodeOutput: string,
): string {
  return `<composer_contract skill="${skill}">
${contract ?? "(no contract found — judge against general digest/answer expectations)"}
</composer_contract>

${nodeBlocks("node_input", nodeInput, "node_output", nodeOutput)}

Score this node's output. Return JSON matching the schema, with exactly these two axes: coverage, composition.`;
}

export function buildFaithUserPrompt(
  contract: string | null,
  nodeInput: string,
  nodeOutput: string,
): string {
  return `<composer_contract>
${contract ?? "(no contract)"}
</composer_contract>

${nodeBlocks("node_input", nodeInput, "node_output", nodeOutput)}

Extract F's atomic factual claims (F = node_output) and verify each against R (R = node_input). Return JSON per the schema.`;
}

// Rubric selector — maps a node kind to its system prompt, the strict response
// schema, and whether the faithfulness sub-judge runs. `compose` and `agent`
// share the composer rubric (both: input -> final text).
export interface NodeRubric {
  system: string;
  responseSchema: Record<string, unknown>;
  buildUserPrompt: (skill: string, contract: string | null, input: string, output: string) => string;
  faithfulness: boolean;
}

export function rubricFor(kind: NodeKind): NodeRubric {
  if (kind === "planner") {
    return {
      system: PLANNER_NODE_PROMPT,
      responseSchema: PLANNER_RESPONSE_SCHEMA,
      buildUserPrompt: (_skill, contract, input, output) =>
        buildPlannerUserPrompt(contract, input, output),
      faithfulness: false,
    };
  }
  return {
    system: COMPOSER_NODE_PROMPT,
    responseSchema: COMPOSER_RESPONSE_SCHEMA,
    buildUserPrompt: (skill, contract, input, output) =>
      buildComposerUserPrompt(skill, contract, input, output),
    faithfulness: true,
  };
}

// The minimal node shape a judge needs — a structural subset of NodeMaterial,
// so the judge functions don't depend on materials.ts (and its trace IO).
export interface JudgeNodeInput {
  kind: NodeKind;
  skill: string;
  contract: string | null;
  inputText: string;
  outputText: string;
}

// One node's verdict: the axis scorecard + (for compose/agent) the
// faithfulness pass. `faithfulness` is null for planner nodes.
export interface NodeJudgement {
  scorecard: Scorecard;
  faithfulness: Faithfulness | null;
}

// ═══════════════════════════════════════════════════════════════════
// Patch
// ═══════════════════════════════════════════════════════════════════

// Gate-side patch injection. The core primitive `appendPatch` lives in
// ./agent-loop (the skill-overlay home) and is the SAME function prod runtime
// uses (workflow.ts compile/execute), so the gate and prod cannot drift. This
// section adds only the chat-message variant the replay needs. It used to
// re-export `appendPatch`/`PATCH_MARKER` so gate-side callers had one import;
// that re-export is gone — every caller now names ./agent-loop directly.


export interface ChatMessage {
  role: string;
  content: unknown;
}

function asText(content: unknown): string {
  return typeof content === "string" ? content : JSON.stringify(content);
}

// Apply a patch to a recorded chat-message array by appending it to the FIRST
// system message (the skill body for a compose node; the body+tools+skills
// prompt for a planner node). Returns a NEW array; throws when there is no
// system message to attach to (a malformed recording the gate can't replay).
export function patchMessages(messages: ChatMessage[], patch: string): ChatMessage[] {
  const idx = messages.findIndex((m) => m.role === "system");
  if (idx < 0) throw new Error("recorded input has no system message to patch");
  const sys = messages[idx]!;
  const patched: ChatMessage = { ...sys, content: appendPatch(asText(sys.content), patch) };
  return messages.map((m, i) => (i === idx ? patched : m));
}

// ═══════════════════════════════════════════════════════════════════
// Monitor
// ═══════════════════════════════════════════════════════════════════

// Live-trend monitor + patch bookkeeping (Phase 3, п3), all pure. The gate can
// be fooled (it optimizes the judge's score on a frozen cluster); PROD is the
// ground truth. After a patch ships, the cron worker gathers the target axis's
// scores on traces that ran AFTER the ship and asks `decideRevert` whether the
// live trend held above the pre-ship baseline. If it confidently fell, the
// worker removes the lesson. The IO (reading post-ship judgements, deleting the
// file) lives in the worker; this module just does the stats + string surgery.

export function mean(xs: number[]): number | null {
  return xs.length === 0 ? null : xs.reduce((a, b) => a + b, 0) / xs.length;
}

export interface Baseline {
  mean: number;
  n: number;
}

export type RevertDecision = "insufficient" | "keep" | "revert";

export interface RevertVerdict {
  decision: RevertDecision;
  postMean: number | null;
  postN: number;
  baselineMean: number;
  // The two-sample noise band the drop had to clear to count as a real regression.
  margin: number | null;
}

// Decide whether a shipped patch's live trend regressed below its pre-ship
// baseline. Conservative by construction: we only revert on a CONFIDENT drop
// (baseline − post beyond k·σ·√(1/postN + 1/baselineN), the two-sample noise
// band), never on a flat trend — a patch that passed the gate and merely fails
// to help in prod is harmless and stays. `minN` guards against reverting on a
// handful of post-ship traces; below it we keep watching ("insufficient").
export function decideRevert(
  baseline: Baseline,
  postScores: number[],
  sigma: number | null,
  k: number,
  minN: number,
): RevertVerdict {
  const postN = postScores.length;
  const base: RevertVerdict = {
    decision: "insufficient",
    postMean: mean(postScores),
    postN,
    baselineMean: baseline.mean,
    margin: null,
  };
  if (postN < minN) return base;
  const postMean = mean(postScores);
  if (postMean === null) return base;
  const margin = sigma === null ? 0 : k * sigma * Math.sqrt(1 / postN + 1 / Math.max(1, baseline.n));
  const drop = baseline.mean - postMean;
  return { ...base, margin, decision: drop > margin ? "revert" : "keep" };
}

// ─── patch lessons (append-only blocks separated by a blank line) ────

// Split a .patch.md into its appended lesson blocks (the unit improve.ts joins
// with a blank line). Empty/whitespace input → no lessons.
export function splitLessons(patch: string): string[] {
  return patch
    .split(/\n\s*\n/)
    .map((b) => b.trim())
    .filter((b) => b.length > 0);
}

export function countLessons(patch: string): number {
  return splitLessons(patch).length;
}

// True when adding one more lesson would exceed the per-skill budget — the guard
// against the giant-prompt regression as lessons accumulate.
export function budgetExceeded(patch: string, maxLessons: number): boolean {
  return countLessons(patch) >= maxLessons;
}

// Surgically remove ONE lesson (the auto-revert of a single bad ship), leaving
// the other lessons intact. Returns the rebuilt patch ("" if nothing remains —
// the caller deletes the file then). Matching is on trimmed text.
export function removeLesson(patch: string, lesson: string): string {
  const want = lesson.trim();
  const kept = splitLessons(patch).filter((b) => b !== want);
  return kept.length === 0 ? "" : `${kept.join("\n\n")}\n`;
}

// ═══════════════════════════════════════════════════════════════════
// Trace source
// ═══════════════════════════════════════════════════════════════════

// Where the judge reads runs from. Two implementations — the local mirror
// (fast, complete, Langfuse-independent) and the Langfuse public API — behind
// one interface so material assembly is source-agnostic. The online worker
// reads local (the forward-looking corpus the self-improvement loop is built
// on); the manual CLI keeps reading Langfuse.

export interface TraceSource {
  getTrace(id: string): Promise<{ trace: TraceRecord; observations: Observation[] }>;
  // Newest-first. `unjudgedFor` (local only) returns just the runs without a
  // judgement for that (provider, promptVersion) — the dedup lives in the
  // query, not a separate KV.
  recentTraces(
    limit: number,
    unjudgedFor?: { provider: string; promptVersion: string },
  ): Promise<TraceSummary[]>;
}

export function createLangfuseTraceSource(): TraceSource {
  return {
    getTrace: (id) => fetchTraceById(id),
    recentTraces: (limit) => fetchRecentTraces(limit),
  };
}

// Reads from the local store. A trace is only written on trace.end(), so
// anything in the store is a COMPLETE run — no "is it still running?" age
// filter needed (unlike the Langfuse list, which can surface mid-run traces).
// `fallback` (e.g. Langfuse) covers getTrace for ids not mirrored locally.
export function createLocalTraceSource(store: TraceStore, fallback?: TraceSource): TraceSource {
  return {
    async getTrace(id) {
      const local = store.getTrace(id);
      if (local) return local;
      if (fallback) return fallback.getTrace(id);
      throw new Error(`trace ${id} not in local store and no fallback configured`);
    },
    async recentTraces(limit, unjudgedFor) {
      return store.listRecent(limit, unjudgedFor);
    },
  };
}

// ═══════════════════════════════════════════════════════════════════
// Noise
// ═══════════════════════════════════════════════════════════════════

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

function meanNonEmpty(xs: number[]): number {
  return xs.reduce((a, b) => a + b, 0) / xs.length;
}

// Sample stddev (n-1). 0 for fewer than two samples (no spread observable).
export function sampleStdDev(xs: number[]): number {
  if (xs.length < 2) return 0;
  const m = meanNonEmpty(xs);
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
  // sqrt(meanNonEmpty of per-node sample variance) — the pooled repeatability σ, the
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
      pooledSigma: variances.length ? round(Math.sqrt(meanNonEmpty(variances))) : 0,
      meanSigma: sigmas.length ? round(meanNonEmpty(sigmas)) : 0,
      p90Sigma: round(percentile(sigmas, 90)),
      maxSigma: sigmas.length ? round(Math.max(...sigmas)) : 0,
      meanScore: allScores.length ? round(meanNonEmpty(allScores), 3) : 0,
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

// ═══════════════════════════════════════════════════════════════════
// Print
// ═══════════════════════════════════════════════════════════════════

// Header line for a judged node: its display label, kind, and owner skill.
export interface NodeHeader {
  label: string;
  kind: string;
  skill: string;
}

export function printTraceHeader(traceId: string): void {
  console.log(
    `\n=== JUDGE ${JUDGE_MODEL} (prompt ${JUDGE_PROMPT_VERSION}) · trace ${traceId} ===`,
  );
}

// One block per node: the axis scorecard (planner: query_formulation/process;
// composer/agent: coverage/composition) and, for compose/agent, faithfulness.
export function printNodeJudgement(node: NodeHeader, j: NodeJudgement): void {
  console.log(`\n── node ${node.label} · ${node.kind} · skill ${node.skill} ──`);
  for (const a of j.scorecard.axes) {
    const score = a.applicable && a.score !== null ? a.score.toFixed(2) : "n/a";
    console.log(`● ${a.axis}: ${a.label} (${score})`);
    console.log(`  ${a.rationale}`);
    if (a.evidence) console.log(`  ↳ ${a.evidence}`);
  }
  console.log(`  overall: ${j.scorecard.overall_note}`);
  if (j.faithfulness) printFaithfulness(j.faithfulness);
}

function printFaithfulness(f: Faithfulness): void {
  if (!f.applicable) {
    console.log(`● faithfulness: n/a — ${f.note}`);
    return;
  }
  const score = f.score !== null ? f.score.toFixed(2) : "—";
  const bad = f.claims.filter((c) => c.verdict !== "supported").length;
  console.log(`● faithfulness: ${score}  (${f.claims.length} claims, ${bad} not fully supported)`);
  for (const c of f.claims) {
    const mark = c.verdict === "supported" ? "✓" : c.verdict === "partial" ? "~" : "✗";
    console.log(`  ${mark} ${c.claim}`);
    if (c.verdict !== "supported") console.log(`      ↳ ${c.evidence}`);
  }
  if (f.note) console.log(`  ${f.note}`);
}

// Compact one-liner per node for a multi-node / multi-trace summary table:
//   node llm_compose:digest · compose · skill news-digest · coverage 0.80 composition 0.68 faithfulness 0.91
export function nodeSummaryLine(node: NodeHeader, j: NodeJudgement): string {
  const parts: string[] = [];
  for (const a of j.scorecard.axes) {
    if (a.applicable && a.score !== null) parts.push(`${a.axis} ${a.score.toFixed(2)}`);
  }
  if (j.faithfulness?.applicable && j.faithfulness.score !== null) {
    parts.push(`faithfulness ${j.faithfulness.score.toFixed(2)}`);
  }
  return `node ${node.label} · ${node.kind} · skill ${node.skill} · ${parts.join(" ") || "n/a"}`;
}

// ═══════════════════════════════════════════════════════════════════
// Sigma baseline
// ═══════════════════════════════════════════════════════════════════

// Shared reader for the committed judge-noise baseline
// (./judging-noise-baseline.json,
// written by `pnpm judge:noise`). The gate, the improver, and the cron worker all
// read the SAME per-(judge model | prompt version) σ floor from here so they
// can't drift. `found` lets a caller warn when there's no baseline (verdicts then
// read "no-baseline").

const BASELINE_PATH = "packages/agent/src/judging-noise-baseline.json";

interface BaselineEntry {
  axes: Array<{ axis: NoiseAxis; pooledSigma: number }>;
}

export function loadSigmaBaseline(
  judgeModel: string,
  promptVersion: string = JUDGE_PROMPT_VERSION,
): { sigma: Partial<Record<NoiseAxis, number>>; found: boolean } {
  let entries: Record<string, BaselineEntry>;
  try {
    entries = JSON.parse(readFileSync(BASELINE_PATH, "utf-8")) as Record<string, BaselineEntry>;
  } catch {
    return { sigma: {}, found: false };
  }
  const entry = entries[`${judgeModel}|${promptVersion}`];
  if (!entry) return { sigma: {}, found: false };
  const sigma: Partial<Record<NoiseAxis, number>> = {};
  for (const a of entry.axes) sigma[a.axis] = a.pooledSigma;
  return { sigma, found: true };
}

// ═══════════════════════════════════════════════════════════════════
// Judge backend
// ═══════════════════════════════════════════════════════════════════

// A judge backend runs ONE structured-output completion: a system instruction
// + user content + a strict JSON schema → the raw parsed JSON object, which the
// caller validates with Zod. This is the SINGLE primitive that differs between
// judge providers (OpenAI chat vs the codex service). Everything provider-
// agnostic — rubric selection, prompt building, the scorecard+faithfulness
// split — lives in node-judge.ts and depends only on this interface.
export interface JudgeCompletion {
  // Schema name (OpenAI's json_schema label, e.g. "scorecard"); ignored by
  // backends that don't take one.
  name: string;
  system: string;
  user: string;
  schema: Record<string, unknown>;
}

export interface JudgeBackend {
  complete(req: JudgeCompletion): Promise<unknown>;
}

export function createOpenAiJudgeBackend(openai: OpenAI): JudgeBackend {
  return {
    async complete({ name, system, user, schema }) {
      const res = await openai.chat.completions.create({
        model: JUDGE_MODEL,
        messages: [
          { role: "system", content: system },
          { role: "user", content: user },
        ],
        response_format: { type: "json_schema", json_schema: { name, strict: true, schema } },
      });
      const content = res.choices[0]?.message.content;
      if (!content) throw new Error("judge returned empty content");
      return JSON.parse(content);
    },
  };
}

export function createCodexJudgeBackend(codex: CodexClient): JudgeBackend {
  const timeoutMs = Number(process.env.CODEX_JUDGE_TIMEOUT_MS ?? 10 * 60_000);
  return {
    async complete({ system, user, schema }) {
      const result = await codex.run({
        prompt: `${system}\n\nReturn only the final JSON object matching the provided schema.`,
        input: user,
        schema,
        sandbox: "read-only",
        approvalPolicy: "never",
        timeoutMs,
        config: {
          web_search: "disabled",
          "features.shell_tool": false,
          "features.multi_agent": false,
        },
      });
      // The codex service may pre-parse to `parsed`; fall back to the raw text.
      return result.parsed !== undefined ? result.parsed : JSON.parse(result.content);
    },
  };
}

export type JudgeProvider = "openai" | "codex";

function openAiFromEnv(): OpenAI {
  const apiKey = process.env.OPENAI_API_KEY;
  if (!apiKey) throw new Error("OPENAI_API_KEY missing in env");
  return new OpenAI({ apiKey });
}

// Resolve a provider name to its backend. `openai` may be passed by a caller
// that already built a client (the CLI); otherwise it's created from env.
export function createJudgeBackend(provider: JudgeProvider, openai?: OpenAI | null): JudgeBackend {
  if (provider === "codex") return createCodexJudgeBackend(createCodexClient());
  return createOpenAiJudgeBackend(openai ?? openAiFromEnv());
}

// ═══════════════════════════════════════════════════════════════════
// Node judge
// ═══════════════════════════════════════════════════════════════════

// Provider-agnostic per-node judging. Selects the rubric by node kind, builds
// the prompts, and validates the backend's JSON — the same for every provider.
// The only provider-specific part (running the completion) is the injected
// JudgeBackend.

async function scorecardFor(backend: JudgeBackend, node: JudgeNodeInput): Promise<Scorecard> {
  const rubric = rubricFor(node.kind);
  const json = await backend.complete({
    name: "scorecard",
    system: rubric.system,
    user: rubric.buildUserPrompt(node.skill, node.contract, node.inputText, node.outputText),
    schema: rubric.responseSchema,
  });
  return ScorecardSchema.parse(json);
}

async function faithfulnessFor(backend: JudgeBackend, node: JudgeNodeInput): Promise<Faithfulness> {
  const json = await backend.complete({
    name: "faithfulness",
    system: FAITH_SYSTEM_PROMPT,
    user: buildFaithUserPrompt(node.contract, node.inputText, node.outputText),
    schema: FAITH_RESPONSE_SCHEMA,
  });
  return FaithfulnessSchema.parse(json);
}

// Judge ONE node: its axis scorecard, plus the faithfulness pass for
// compose/agent nodes (planner nodes have no faithfulness axis). The two
// completions run concurrently.
//
// `skipFaithfulness` is the gate's cost knob (target-axis-only judging): the
// faithfulness pass is a SEPARATE backend call over the node's (often large)
// input, so when the improver gates a non-faithfulness axis it can drop it and
// halve the codex cost per sample. The judge-worker never skips — the stored
// corpus keeps all axes.
export async function judgeNode(
  backend: JudgeBackend,
  node: JudgeNodeInput,
  opts: { skipFaithfulness?: boolean } = {},
): Promise<NodeJudgement> {
  const wantsFaith = rubricFor(node.kind).faithfulness && !opts.skipFaithfulness;
  const [scorecard, faithfulness] = await Promise.all([
    scorecardFor(backend, node),
    wantsFaith ? faithfulnessFor(backend, node) : Promise.resolve(null),
  ]);
  return { scorecard, faithfulness };
}

// ═══════════════════════════════════════════════════════════════════
// Materials
// ═══════════════════════════════════════════════════════════════════

const skillStore = createSkillStore();

function stringify(x: unknown): string {
  if (x === null || x === undefined) return "";
  if (typeof x === "string") return x;
  try {
    return JSON.stringify(x, null, 2);
  } catch {
    return String(x);
  }
}

// Render a node's recorded input/output for the judge. A generation's input is
// a chat-messages array (system + user) — flatten it to role-labelled blocks
// so the judge reads the contract/system and the candidates the way the model
// saw them. Everything else (a span's input, a plain output string) stringifies.
function renderIo(value: unknown): string {
  if (
    Array.isArray(value) &&
    value.length > 0 &&
    value.every((m) => m !== null && typeof m === "object" && "role" in m)
  ) {
    return (value as Array<{ role: unknown; content: unknown }>)
      .map((m) => `[${String(m.role)}]\n${stringify(m.content)}`)
      .join("\n\n");
  }
  return stringify(value);
}

// One generative node lifted out of the observation tree, ready to judge. The
// `skill` is the OWNER (a named skill, or "planner" for the planner node and
// for prompt-only composes the planner authored); the improver aggregates by
// it. `contract` is that owner's rubric text. `inputText`/`outputText` are the
// node's own recorded IO — the judge scores this node in isolation, never the
// whole run.
export interface NodeMaterial {
  observationId: string;
  kind: NodeKind;
  skill: string;
  // Display label for the CLI scorecard — the observation name
  // (e.g. "attempt-1", "llm_compose:digest", "step[2]:llm_agent").
  label: string;
  contract: string | null;
  inputText: string;
  outputText: string;
}

const ROOT_SKIP = Symbol("root");

// True when any ancestor of `obs` is a SPAWNED sub-agent AGENT span — i.e.
// `obs` is a generation/span INSIDE an llm_agent step (its `iter-*` calls),
// which we judge only black-box at the agent-step boundary, never node by node.
// The trace ROOT is itself an AGENT span (the supervisor / agent-loop start the
// trace with kind:"agent"), but it is NOT a spawned sub-agent — every node
// lives under it. So a root AGENT (parentObservationId === null) does NOT count;
// only a NESTED AGENT (an actual llm_agent step) black-boxes its descendants.
function hasAgentAncestor(obs: Observation, byId: Map<string, Observation>): boolean {
  let parent = obs.parentObservationId ? byId.get(obs.parentObservationId) : undefined;
  while (parent) {
    if (parent.type === "AGENT" && parent.parentObservationId !== null) return true;
    parent = parent.parentObservationId ? byId.get(parent.parentObservationId) : undefined;
  }
  return false;
}

// Walk the observation tree and classify every JUDGEABLE generative node.
// Classification is by an explicit metadata contract, NEVER by observation
// name (names are display-only and can be renamed freely):
//   - generation tagged judge_node="planner"  → planner node (owner planner)
//   - generation tagged judge_node="compose"  → compose node; owner = the
//                                                tagged-along metadata.skill, or
//                                                planner when prompt-only (null)
//   - AGENT-type span (an llm_agent step)      → agent node, judged black-box
//                                                (input→output); its inner
//                                                generations are skipped
// Tool spans, embeddings, events, the trace root, untagged generations, and
// pure container spans (planner/runner/step/parallel) are skipped.
export async function assembleNodeMaterials(
  source: TraceSource,
  traceId: string,
): Promise<{ trace: TraceRecord; nodes: NodeMaterial[] }> {
  const { trace, observations } = await source.getTrace(traceId);
  const byId = new Map(observations.map((o) => [o.id, o]));

  const plannerContract = await skillStore.readSkillRaw("planner");
  // Memoize composer-skill reads — a run can have several compose nodes on the
  // same skill (map stage), and the same skill across nodes.
  const contractCache = new Map<string, string | null>([["planner", plannerContract]]);
  const readContract = async (skill: string): Promise<string | null> => {
    if (!contractCache.has(skill)) contractCache.set(skill, await skillStore.readSkillRaw(skill));
    return contractCache.get(skill) ?? null;
  };

  // Sort by start time so the scorecard reads in execution order.
  const sorted = [...observations].sort(
    (a, b) => new Date(a.startTime).getTime() - new Date(b.startTime).getTime(),
  );

  const nodes: NodeMaterial[] = [];
  for (const o of sorted) {
    const classified = classify(o, trace, byId);
    if (classified === ROOT_SKIP || classified === null) continue;

    const skill = classified.skill;
    const contract = skill === "planner" ? plannerContract : await readContract(skill);
    nodes.push({
      observationId: o.id,
      kind: classified.kind,
      skill,
      label: o.name,
      contract,
      inputText: renderIo(o.input),
      outputText: renderIo(o.output),
    });
  }

  return { trace, nodes };
}

// Pure classification of one observation. Returns the node kind + owner skill,
// null to skip, or ROOT_SKIP for the trace root. Kept separate so tests can
// pin the rules without rendering or skill IO.
export function classify(
  o: Observation,
  trace: Pick<TraceRecord, "name">,
  byId: Map<string, Observation>,
): { kind: NodeKind; skill: string } | null | typeof ROOT_SKIP {
  if (o.parentObservationId === null && o.name === trace.name) return ROOT_SKIP;

  // An llm_agent step IS an AGENT span — judged black-box. A nested agent (an
  // agent spawned inside another) is already covered by the outer black box.
  if (o.type === "AGENT") {
    if (hasAgentAncestor(o, byId)) return null;
    return { kind: "agent", skill: ownerSkill(o.metadata?.skill) };
  }

  if (o.type !== "GENERATION") return null;
  if (hasAgentAncestor(o, byId)) return null;

  const role = o.metadata?.[JUDGE_NODE_META];
  if (role === "planner") return { kind: "planner", skill: "planner" };
  // Compose node carries its own owner skill (null → prompt-only → planner).
  if (role === "compose") return { kind: "compose", skill: ownerSkill(o.metadata?.skill) };

  return null;
}

// A node's owner: the named skill if present, else the planner (prompt-only
// composes and any node whose skill is absent are the planner's responsibility).
function ownerSkill(skill: unknown): string {
  return typeof skill === "string" && skill.length > 0 ? skill : "planner";
}

export { ROOT_SKIP };

// ═══════════════════════════════════════════════════════════════════
// Langfuse scores
// ═══════════════════════════════════════════════════════════════════

// Persists ONE node's verdict to two places: the local `judgements` table (the
// per-node corpus the self-improvement loop queries) and Langfuse scores (the
// UI / dashboards). The local PK is (trace, observation), and the Langfuse
// score carries `observationId` = the judged node, so both link with no
// mapping and scores render ON THE STEP in the trace UI. dryRun prints and
// persists NOTHING — so flipping JUDGE_WRITE_SCORES on later re-judges every
// node instead of finding them already recorded.

export interface NodeScoreWriteOpts {
  traceId: string;
  observationId: string;
  nodeKind: NodeKind;
  skill: string;
  provider: "openai" | "codex";
  promptVersion: string;
  dryRun: boolean;
}

export interface ScoreWriter {
  // `faith` is null for planner nodes (no faithfulness axis).
  write(card: Scorecard, faith: Faithfulness | null, opts: NodeScoreWriteOpts): Promise<void>;
}

interface LangfuseScorePayload {
  traceId: string;
  observationId: string;
  name: string;
  value: number;
  comment?: string;
  metadata?: Record<string, unknown>;
}

function comment(...parts: Array<string | null | undefined>): string {
  return parts.filter((p): p is string => Boolean(p)).join("\n");
}

// Pull the numeric value for one axis (null when absent from this node's rubric
// or marked n/a — applicable=false or score=null).
function axisValue(card: Scorecard, axis: string): number | null {
  const a = card.axes.find((x) => x.axis === axis);
  return a && a.applicable && a.score !== null ? a.score : null;
}

// The five axis columns the local row carries. A node fills only the axes its
// rubric emits (planner: query_formulation/process; composer/agent:
// coverage/composition/faithfulness); the rest stay null.
function axisScores(card: Scorecard, faith: Faithfulness | null) {
  return {
    query_formulation: axisValue(card, "query_formulation"),
    process: axisValue(card, "process"),
    coverage: axisValue(card, "coverage"),
    composition: axisValue(card, "composition"),
    faithfulness: faith && faith.applicable && faith.score !== null ? faith.score : null,
  };
}

function scorePayloads(
  card: Scorecard,
  faith: Faithfulness | null,
  opts: Pick<NodeScoreWriteOpts, "traceId" | "observationId" | "nodeKind" | "skill" | "provider" | "promptVersion">,
): LangfuseScorePayload[] {
  const baseMeta = {
    judge_provider: opts.provider,
    judge_prompt_version: opts.promptVersion,
    node_kind: opts.nodeKind,
    skill: opts.skill,
  };
  const payloads: LangfuseScorePayload[] = [];
  for (const axis of card.axes) {
    if (!axis.applicable || axis.score === null) continue;
    payloads.push({
      traceId: opts.traceId,
      observationId: opts.observationId,
      name: `judge.${axis.axis}`,
      value: axis.score,
      comment: comment(axis.label, axis.rationale, axis.evidence),
      metadata: { ...baseMeta, label: axis.label },
    });
  }
  if (faith && faith.applicable && faith.score !== null) {
    payloads.push({
      traceId: opts.traceId,
      observationId: opts.observationId,
      name: "judge.faithfulness",
      value: faith.score,
      comment: comment(
        faith.note,
        faith.claims
          .filter((c) => c.verdict !== "supported")
          .map((c) => `${c.verdict}: ${c.claim} (${c.evidence})`)
          .join("\n"),
      ),
      metadata: {
        ...baseMeta,
        claim_count: faith.claims.length,
        unsupported_count: faith.claims.filter((c) => c.verdict === "unsupported").length,
        partial_count: faith.claims.filter((c) => c.verdict === "partial").length,
      },
    });
  }
  return payloads;
}

export function createScoreWriter(deps: {
  store: TraceStore;
  langfuseEnabled: boolean;
}): ScoreWriter {
  return {
    async write(card, faith, opts) {
      const payloads = scorePayloads(card, faith, opts);
      if (opts.dryRun) {
        for (const p of payloads) {
          console.log(`[judge] dry-run score ${p.traceId}/${p.observationId} ${p.name}=${p.value}`);
        }
        return;
      }

      // Local corpus first — it's the source of truth for the improver and
      // doesn't depend on Langfuse being up.
      deps.store.writeJudgement({
        traceId: opts.traceId,
        observationId: opts.observationId,
        nodeKind: opts.nodeKind,
        skill: opts.skill,
        provider: opts.provider,
        promptVersion: opts.promptVersion,
        scores: axisScores(card, faith),
        detail: { scorecard: card, faithfulness: faith },
      });

      if (!deps.langfuseEnabled) {
        console.log(
          `[judge] wrote local judgement ${opts.traceId}/${opts.observationId} (langfuse disabled)`,
        );
        return;
      }
      for (const p of payloads) {
        await apiPost<unknown>("/scores", p);
        console.log(`[judge] wrote score ${p.traceId}/${p.observationId} ${p.name}=${p.value}`);
      }
    },
  };
}

// ═══════════════════════════════════════════════════════════════════
// Gate
// ═══════════════════════════════════════════════════════════════════

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
  skipFaithfulness: boolean,
): Promise<Record<NoiseAxis, number[]>> {
  const acc = emptyAxisArrays();
  for (const outputText of outputs) {
    const verdict = await judgeNode(
      deps.backend,
      {
        kind: target.kind,
        skill: target.skill,
        contract: target.contract,
        inputText: target.inputText,
        outputText,
      },
      { skipFaithfulness },
    );
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
  skipFaithfulness = false,
): Promise<NodeGateResult> {
  // after: generate `samples` fresh outputs from the patched prompt, judge each.
  const patchedMessages = patchMessages(target.recordedInput, patch);
  const patchedOutputs: string[] = [];
  for (let i = 0; i < samples; i++) {
    patchedOutputs.push(await deps.runModel(patchedMessages, target.model, target.jsonMode));
  }
  const after = await judgeOutputs(deps, target, patchedOutputs, skipFaithfulness);

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

// ═══════════════════════════════════════════════════════════════════
// Gate runtime
// ═══════════════════════════════════════════════════════════════════

// Generator-replay runtime for the gate / improver: re-running the generator
// under the recorded model, and lifting a NodeMaterial + its observation into a
// replayable GateNodeTarget. Shared by the improve cycle, the improve worker,
// and judge-gate.ts so they can't drift.

// Lazy, per-provider — built on first use (after the caller has loaded env) and
// only for the provider actually needed, so a deepseek-only run never requires a
// GEMINI/OPENAI key just to import this module.
let openaiClient: OpenAI | undefined;
let deepseekClient: OpenAI | undefined;
let geminiClient: OpenAI | undefined;

export function clientFor(model: string): OpenAI {
  if (model.startsWith("deepseek")) {
    return (deepseekClient ??= new OpenAI({ apiKey: process.env.DEEPSEEK_API_KEY, baseURL: DEEPSEEK_BASE_URL }));
  }
  if (model.startsWith("gemini")) {
    return (geminiClient ??= new OpenAI({ apiKey: process.env.GEMINI_API_KEY, baseURL: GEMINI_BASE_URL }));
  }
  return (openaiClient ??= new OpenAI({ apiKey: process.env.OPENAI_API_KEY }));
}

function toParam(m: ChatMessage): ChatCompletionMessageParam {
  const content = typeof m.content === "string" ? m.content : JSON.stringify(m.content);
  if (m.role === "system") return { role: "system", content };
  if (m.role === "assistant") return { role: "assistant", content };
  return { role: "user", content };
}

// Re-run the generator under the recorded model. jsonMode mirrors production
// (planner emits JSON, composer prose).
export async function runModel(messages: ChatMessage[], model: string, jsonMode: boolean): Promise<string> {
  const res = await retryOnTransient(
    (signal) =>
      clientFor(model).chat.completions.create({
        model,
        messages: messages.map(toParam),
        ...(jsonMode ? { response_format: { type: "json_object" } } : {}),
      }, { signal, maxRetries: 0 }),
    { maxRetries: 5, baseDelayMs: 3000 },
  );
  return res.choices[0]?.message.content ?? "";
}

function recordedMessages(input: unknown): ChatMessage[] {
  if (!Array.isArray(input)) return [];
  return input.filter((m): m is ChatMessage => typeof m === "object" && m !== null && "role" in m);
}

// Lift a judged node + its recorded observation into a replayable target.
// Returns null when no generator model can be resolved (can't replay).
export function buildGateTarget(node: NodeMaterial, obs: Observation | undefined): GateNodeTarget | null {
  const model = obs?.model ?? process.env.AGENT_MODEL;
  if (!model) return null;
  return {
    observationId: node.observationId,
    kind: node.kind,
    skill: node.skill,
    label: node.label,
    contract: node.contract,
    inputText: node.inputText,
    originalOutput: node.outputText,
    model,
    recordedInput: recordedMessages(obs?.input),
    jsonMode: node.kind === "planner",
  };
}

// ═══════════════════════════════════════════════════════════════════
// Improver
// ═══════════════════════════════════════════════════════════════════

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

// ═══════════════════════════════════════════════════════════════════
// Improve cycle
// ═══════════════════════════════════════════════════════════════════

// `runModel` from the Gate runtime section above, kept under the name this
// section used when it imported it aliased: `runImproveCycle` binds a local
// `runModel` (deps override ?? default) that would otherwise shadow it.
const defaultRunModel = runModel;

// ONE improvement cycle for a single (skill, axis): select recent failures →
// induce a failure-mode taxonomy → author an append-only lesson for the dominant
// mode (with ≤1 informed retry on a failed gate) → cheap-gate it (stored before,
// cluster S× + holdout S=1) → decide → ship. Pure orchestration over injected
// deps so BOTH the CLI (scripts/improve.ts) and the cron worker
// (scripts/improve-worker.ts) drive the exact same logic. The live-trend monitor
// and auto-revert live in the worker (they span runs); this is one cycle.

// Pull the judge's rationale for one axis out of the stored detail JSON.
const DetailSchema = z.object({
  scorecard: z
    .object({
      axes: z.array(
        z.object({ axis: z.string(), rationale: z.string().optional(), evidence: z.string().optional() }),
      ),
    })
    .optional(),
  faithfulness: z.object({ note: z.string().optional() }).nullable().optional(),
});

export function judgeRationale(detail: unknown, axis: NoiseAxis): string {
  const p = DetailSchema.safeParse(detail);
  if (!p.success) return "";
  if (axis === "faithfulness") return p.data.faithfulness?.note ?? "";
  const a = p.data.scorecard?.axes.find((x) => x.axis === axis);
  if (!a) return "";
  return `${a.rationale ?? ""}${a.evidence ? ` (evidence: ${a.evidence})` : ""}`.trim();
}

export type CycleOutcome =
  | "no-corpus" // nothing judged for this (skill, axis) yet
  | "no-candidates" // floor reached the ceiling — no confident recent failures
  | "no-mode" // taxonomy/material yielded nothing patchable
  | "no-fix" // author found no generalizable lesson
  | "rejected" // candidate(s) failed the gate
  | "budget" // accepted, but the per-skill patch budget is full — not shipped
  | "accepted" // passed the gate, but apply=false (propose-only)
  | "shipped"; // applied to skills/<skill>.patch.md

export interface CycleResult {
  outcome: CycleOutcome;
  mode?: string;
  lesson?: string;
  reasons?: string[];
  // Pre-ship recent axis mean + count — the baseline the worker's live monitor
  // compares post-ship prod traces against. Set on "shipped".
  baseline?: { mean: number; n: number };
  patchPath?: string;
}

export interface ImproveCycleOpts {
  skill: string;
  axis: NoiseAxis;
  provider: JudgeProvider;
  cluster: number;
  holdout: number;
  samples: number; // cluster samples (S); holdout is always S=1
  absMax: number;
  bar: number;
  holdoutMin: number;
  recentDays: number;
  k: number;
  apply: boolean;
  maxAttempts: number; // 1 → no retry; 2 → one informed retry after a failed gate
  budget: number; // per-skill patch lesson budget (block new ships when full)
  // Re-judge the faithfulness axis in the gate too (collateral-regression guard).
  // Default OFF: faithfulness is a separate, input-heavy codex call — skipping it
  // when it isn't the target halves the gate's codex cost (target-axis-only).
  guardFaithfulness: boolean;
  now: number; // Date.now(), injected for the recent window + testability
}

export interface ImproveCycleDeps {
  store: Pick<TraceStore, "listJudgements">;
  source: TraceSource;
  skillStore: Pick<SkillStore, "readPatch" | "savePatch">;
  backend: JudgeBackend;
  sigma: Partial<Record<NoiseAxis, number>>;
  runModel?: (messages: Parameters<typeof defaultRunModel>[0], model: string, jsonMode: boolean) => Promise<string>;
  log?: (msg: string) => void;
}

const nid = (r: { traceId: string; observationId: string }): string => `${r.traceId}:${r.observationId}`;

// Compact feedback for the informed retry: why the previous lesson failed the
// gate, per cluster node (before→after on the target axis + verdict).
function gateFeedback(reasons: string[], cluster: NodeGateResult[], axis: NoiseAxis): string {
  const lines = cluster.map((n) => {
    const g = n.grades.find((x) => x.axis === axis);
    if (!g) return `- ${n.node.label}: ${axis} not scored`;
    const f = (x: number | null) => (x === null ? "—" : x.toFixed(2));
    return `- ${n.node.label}: ${axis} ${f(g.beforeMean)}→${f(g.afterMean)} [${g.verdict}]`;
  });
  return [`Gate reasons: ${reasons.join("; ")}`, ...lines].join("\n");
}

export async function runImproveCycle(deps: ImproveCycleDeps, opts: ImproveCycleOpts): Promise<CycleResult> {
  const log = deps.log ?? (() => {});
  const runModel = deps.runModel ?? defaultRunModel;
  const { skill, axis } = opts;
  const axisSigma = deps.sigma[axis] ?? null;

  const records = deps.store.listJudgements({
    skill,
    provider: opts.provider,
    promptVersion: JUDGE_PROMPT_VERSION,
  });
  if (records.length === 0) return { outcome: "no-corpus" };

  const recentSince =
    opts.recentDays > 0 ? new Date(opts.now - opts.recentDays * 86_400_000).toISOString() : null;
  const inWindow = (r: JudgementRecord): boolean => recentSince === null || r.startedAt >= recentSince;

  const { candidates, holdout } = selectCandidates(records, axis, {
    holdoutSize: opts.holdout,
    absMax: opts.absMax,
    bar: opts.bar,
    k: opts.k,
    sigma: axisSigma,
    holdoutMin: opts.holdoutMin,
    recentSince,
  });
  log(`corpus=${records.length} · recent candidates=${candidates.length} · holdout=${holdout.length}`);
  if (candidates.length === 0) return { outcome: "no-candidates" };

  // Pre-ship baseline = mean of the axis over ALL recent nodes (not just the
  // failures) — the skill's current live level the monitor will compare against.
  const recentScores = records
    .filter((r) => inWindow(r) && r.scores[axis] !== null)
    .map((r) => r.scores[axis]!);
  const baseMean = mean(recentScores);
  const baseline = baseMean === null ? null : { mean: baseMean, n: recentScores.length };

  // Taxonomy → dominant mode → cluster.
  const rationaleById = new Map(candidates.map((r) => [nid(r), judgeRationale(r.detail, axis)]));
  log(`Inducing failure-mode taxonomy over ${candidates.length} candidate(s)…`);
  const taxonomy = await induceTaxonomy(deps.backend, {
    skill,
    axis,
    items: candidates.map((r) => ({ id: nid(r), rationale: rationaleById.get(nid(r)) ?? "" })),
  });
  for (const m of taxonomy.modes) {
    log(`  · ${m.name} (${m.nodeIds.filter((id) => rationaleById.has(id)).length}): ${m.description}`);
  }
  const mode = dominantMode(taxonomy, new Set(rationaleById.keys()));
  if (!mode) return { outcome: "no-mode" };
  const modeIds = new Set(mode.nodeIds);
  const cluster = candidates.filter((r) => modeIds.has(nid(r))).slice(0, opts.cluster);
  log(`Dominant mode: "${mode.name}" — ${mode.description} · cluster=${cluster.length}`);

  // Resolve cluster + holdout nodes to replayable material (cache per trace).
  const traceCache = new Map<string, { nodes: NodeMaterial[]; byId: Map<string, Observation> }>();
  async function resolve(rec: JudgementRecord): Promise<{ node: NodeMaterial; obs: Observation | undefined } | null> {
    let entry = traceCache.get(rec.traceId);
    if (!entry) {
      const { observations } = await deps.source.getTrace(rec.traceId);
      const { nodes } = await assembleNodeMaterials(deps.source, rec.traceId);
      entry = { nodes, byId: new Map(observations.map((o) => [o.id, o])) };
      traceCache.set(rec.traceId, entry);
    }
    const node = entry.nodes.find((n) => n.observationId === rec.observationId);
    return node ? { node, obs: entry.byId.get(rec.observationId) } : null;
  }

  const examples: PatchExample[] = [];
  let contract: string | null = null;
  for (const rec of cluster) {
    const r = await resolve(rec);
    if (!r) continue;
    contract = r.node.contract;
    examples.push({
      inputExcerpt: r.node.inputText,
      output: r.node.outputText,
      judgeRationale: judgeRationale(rec.detail, axis) || "(no rationale recorded)",
    });
  }
  if (examples.length === 0) return { outcome: "no-mode" };

  const existingPatch = (await deps.skillStore.readPatch(skill)) ?? "";

  // Short-circuit before spending codex on author+gate: if we'd ship but the
  // per-skill budget is full, there's nothing to do until lessons are pruned.
  // (In propose-only mode we still author, to surface the candidate.)
  if (opts.apply && budgetExceeded(existingPatch, opts.budget)) {
    log(`patch budget (${opts.budget}) reached for ${skill} — not authoring; prune lessons or raise the budget.`);
    return { outcome: "budget", mode: mode.description };
  }

  // Target-axis-only judging: only re-judge faithfulness when it IS the target
  // or the caller explicitly guards it — otherwise skip its costly extra pass.
  const skipFaithfulness = axis !== "faithfulness" && !opts.guardFaithfulness;
  async function gateNodes(recs: JudgementRecord[], samples: number, lesson: string): Promise<NodeGateResult[]> {
    const out: NodeGateResult[] = [];
    for (const rec of recs) {
      const r = await resolve(rec);
      if (!r) continue;
      const target = buildGateTarget(r.node, r.obs);
      if (!target) continue;
      out.push(
        await runNodeGate(
          { backend: deps.backend, runModel },
          target,
          lesson,
          samples,
          deps.sigma,
          opts.k,
          rec.scores,
          skipFaithfulness,
        ),
      );
    }
    return out;
  }

  // Author → gate, with ≤(maxAttempts−1) informed retries on a failed gate.
  let priorFeedback: string | null = null;
  let lastReasons: string[] = [];
  let lastLesson: string | null = null;
  for (let attempt = 1; attempt <= Math.max(1, opts.maxAttempts); attempt++) {
    const authored = await authorPatch(deps.backend, {
      skill,
      axis,
      contract,
      examples,
      failureMode: mode.description,
      existingPatch,
      priorFeedback,
    });
    log(`[attempt ${attempt}] candidate: ${authored.lesson.trim() ? authored.lesson.trim() : "(empty)"}`);
    if (authored.lesson.trim().length === 0) return { outcome: "no-fix", mode: mode.description };
    lastLesson = authored.lesson.trim();

    const clusterResults = await gateNodes(cluster, opts.samples, authored.lesson);
    const holdoutResults = await gateNodes(holdout, 1, authored.lesson);
    const decision = decideShip(axis, clusterResults, holdoutResults);
    lastReasons = decision.reasons;
    for (const r of decision.reasons) log(`  • ${r}`);

    if (!decision.accept) {
      priorFeedback = gateFeedback(decision.reasons, clusterResults, axis);
      continue;
    }

    // Accepted. (Budget was already checked before authoring when apply=true.)
    if (!opts.apply) return { outcome: "accepted", mode: mode.description, lesson: authored.lesson, reasons: decision.reasons };
    const merged =
      existingPatch.trim().length > 0
        ? `${existingPatch.trimEnd()}\n\n${authored.lesson.trim()}\n`
        : `${authored.lesson.trim()}\n`;
    const saved = await deps.skillStore.savePatch(skill, merged);
    log(`SHIPPED → ${saved.path} (${saved.sizeBytes} bytes)`);
    return {
      outcome: "shipped",
      mode: mode.description,
      lesson: authored.lesson.trim(),
      reasons: decision.reasons,
      baseline: baseline ?? undefined,
      patchPath: saved.path,
    };
  }

  return { outcome: "rejected", mode: mode.description, lesson: lastLesson ?? undefined, reasons: lastReasons };
}

// ═══════════════════════════════════════════════════════════════════
// Improve worker
// ═══════════════════════════════════════════════════════════════════

// The closed-loop improver's cron driver (Phase 3, п3). On each tickImprover it walks
// every (skill, axis) present in the judged corpus and, per pair:
//   1. MONITOR — if the last ship is still "pending", compare the axis's prod
//      scores on traces that ran AFTER the ship against the pre-ship baseline.
//      Confident drop → AUTO-REVERT (remove the lesson); held → mark "kept";
//      too few post-ship traces → keep watching and DON'T author (one change at
//      a time, so each ship's effect stays attributable).
//   2. IMPROVE — otherwise run one runImproveCycle and record the outcome.
// Mirrors judge-worker: a long-running poll loop, one service in compose. The
// gate is the in-process guard; THIS adds prod as the second, ground-truth guard.

export interface ImproveWorkerDeps {
  store: Pick<TraceStore, "listJudgements" | "listJudgedSkills">;
  improverStore: ImproverStore;
  skillStore: SkillStore;
  source: TraceSource;
  backend: JudgeBackend;
  sigma: Partial<Record<NoiseAxis, number>>;
  log?: (msg: string) => void;
  // Durable, structured audit sink (one record per cycle + monitor event),
  // distinct from `log` (ephemeral stdout lost on container recreate). The
  // script appends these as JSONL on the agent-data volume so a week of runs —
  // INCLUDING rejected proposals (with the lesson + gate reasons) — is reviewable
  // later. No-op when unset.
  audit?: (entry: Record<string, unknown>) => void;
}

export interface ImproveWorkerOpts {
  provider: JudgeProvider;
  apply: boolean; // false → shadow mode (propose + log, never ship). Flip on after prod-validation.
  pollIntervalMs: number;
  once: boolean;
  recentDays: number;
  minMonitorN: number; // min post-ship traces before the monitor will judge a trend
  k: number;
  budget: number;
  cluster: number;
  holdout: number;
  samples: number;
  absMax: number;
  bar: number;
  holdoutMin: number;
  maxAttempts: number;
  guardFaithfulness: boolean;
  // Cost ceiling: at most this many improve cycles (the codex-heavy step) per
  // tickImprover. Monitoring/auto-revert is free and runs for ALL pairs every tickImprover; only
  // AUTHORING is capped. Pairs are picked least-recently-attempted first
  // (round-robin), so an unattended tickImprover can't blow the shared codex quota.
  maxCyclesPerTick: number;
}

function sleep(ms: number): Promise<void> {
  return new Promise((r) => setTimeout(r, ms));
}

function boolEnv(name: string, fallback: boolean): boolean {
  const value = process.env[name];
  if (value === undefined) return fallback;
  return ["1", "true", "yes", "on"].includes(value.toLowerCase());
}

export function improveWorkerOptsFromEnv(): ImproveWorkerOpts {
  const provider: JudgeProvider = process.env.JUDGE_PROVIDER === "openai" ? "openai" : "codex";
  return {
    provider,
    apply: boolEnv("IMPROVE_APPLY", false),
    pollIntervalMs: Number(process.env.IMPROVE_POLL_INTERVAL_MS ?? 86_400_000), // daily; cap below bounds per-tickImprover cost
    once: boolEnv("IMPROVE_ONCE", false),
    recentDays: Number(process.env.IMPROVE_RECENT_DAYS ?? 14),
    minMonitorN: Number(process.env.IMPROVE_MIN_MONITOR_N ?? 5),
    k: Number(process.env.IMPROVE_K ?? 2),
    budget: Number(process.env.IMPROVE_BUDGET ?? 8),
    cluster: Number(process.env.IMPROVE_CLUSTER ?? 3),
    holdout: Number(process.env.IMPROVE_HOLDOUT ?? 3),
    samples: Number(process.env.IMPROVE_SAMPLES ?? 2),
    absMax: Number(process.env.IMPROVE_ABS_MAX ?? 0.6),
    bar: Number(process.env.IMPROVE_BAR ?? 0.75),
    holdoutMin: Number(process.env.IMPROVE_HOLDOUT_MIN ?? 0.85),
    maxAttempts: Number(process.env.IMPROVE_MAX_ATTEMPTS ?? 1), // 1 = no informed retry (retry doubles codex)
    guardFaithfulness: boolEnv("IMPROVE_GUARD_FAITHFULNESS", false),
    maxCyclesPerTick: Number(process.env.IMPROVE_MAX_CYCLES_PER_TICK ?? 1),
  };
}

// Outcome of the monitor step: "watching" / "reverted" / "kept" means we already
// acted on a pending ship; "author" means there is nothing pending so the caller
// should run an improve cycle. "watching" tells the caller to SKIP authoring.
type MonitorAction = "author" | "watching" | "reverted" | "kept";

async function monitorPending(
  deps: ImproveWorkerDeps,
  opts: ImproveWorkerOpts,
  skill: string,
  axis: NoiseAxis,
  records: JudgementRecord[],
  nowISO: string,
): Promise<MonitorAction> {
  const log = deps.log ?? (() => {});
  const state = deps.improverStore.get(skill, axis);
  if (!state || state.monitorStatus !== "pending" || !state.shippedAt || state.baselineMean === null) {
    return "author";
  }

  const post = records
    .filter((r) => r.startedAt > state.shippedAt! && r.scores[axis] !== null)
    .map((r) => r.scores[axis]!);
  const verdict = decideRevert(
    { mean: state.baselineMean, n: state.baselineN ?? 1 },
    post,
    deps.sigma[axis] ?? null,
    opts.k,
    opts.minMonitorN,
  );
  const post3 = verdict.postMean === null ? "—" : verdict.postMean.toFixed(3);
  log(
    `[${skill}/${axis}] monitor: baseline ${state.baselineMean.toFixed(3)} vs post ${post3} (n=${verdict.postN}) → ${verdict.decision}`,
  );

  if (verdict.decision === "insufficient") return "watching";

  if (verdict.decision === "revert") {
    const patch = (await deps.skillStore.readPatch(skill)) ?? "";
    const rebuilt = removeLesson(patch, state.shippedLesson ?? "");
    if (rebuilt.trim().length === 0) await deps.skillStore.deletePatch(skill);
    else await deps.skillStore.savePatch(skill, rebuilt);
    deps.improverStore.upsert(skill, axis, nowISO, {
      lastOutcome: "reverted",
      shippedAt: null,
      shippedLesson: null,
      baselineMean: null,
      baselineN: null,
      monitorStatus: null,
    });
    log(`[${skill}/${axis}] AUTO-REVERTED — live trend fell below baseline; removed the lesson.`);
    deps.audit?.({
      at: nowISO,
      kind: "monitor",
      action: "reverted",
      skill,
      axis,
      baselineMean: state.baselineMean,
      postMean: verdict.postMean,
      postN: verdict.postN,
      lesson: state.shippedLesson ?? null,
    });
    return "reverted";
  }

  // kept: the ship held in prod — settle it (stop monitoring) and let the caller
  // author the next improvement this round.
  deps.improverStore.upsert(skill, axis, nowISO, {
    lastOutcome: "kept",
    shippedAt: null,
    shippedLesson: null,
    baselineMean: null,
    baselineN: null,
    monitorStatus: "kept",
  });
  log(`[${skill}/${axis}] ship held in prod — kept.`);
  deps.audit?.({
    at: nowISO,
    kind: "monitor",
    action: "kept",
    skill,
    axis,
    baselineMean: state.baselineMean,
    postMean: verdict.postMean,
    postN: verdict.postN,
    lesson: state.shippedLesson ?? null,
  });
  return "kept";
}

function recordCycle(
  deps: ImproveWorkerDeps,
  skill: string,
  axis: NoiseAxis,
  // Accepts a CycleResult, plus the worker-level "error" outcome for a cycle
  // that threw — lastOutcome is free text, so this just records the attempt.
  result: Pick<CycleResult, "lesson" | "baseline"> & { outcome: CycleResult["outcome"] | "error" },
  nowISO: string,
): void {
  if (result.outcome === "shipped") {
    deps.improverStore.upsert(skill, axis, nowISO, {
      lastOutcome: "shipped",
      shippedAt: nowISO,
      shippedLesson: result.lesson ?? null,
      baselineMean: result.baseline?.mean ?? null,
      baselineN: result.baseline?.n ?? null,
      monitorStatus: result.baseline ? "pending" : null, // can't monitor without a baseline
    });
    return;
  }
  deps.improverStore.upsert(skill, axis, nowISO, {
    lastOutcome: result.outcome,
    shippedAt: null,
    shippedLesson: null,
    baselineMean: null,
    baselineN: null,
    monitorStatus: null,
  });
}

interface Pair {
  skill: string;
  axis: NoiseAxis;
  records: JudgementRecord[];
  lastAttemptAt: string; // "" = never attempted → sorts first (round-robin)
}

async function tickImprover(deps: ImproveWorkerDeps, opts: ImproveWorkerOpts): Promise<void> {
  const log = deps.log ?? (() => {});
  const skills = deps.store.listJudgedSkills({ provider: opts.provider, promptVersion: JUDGE_PROMPT_VERSION });
  log(`[improve-worker] ${skills.length} skill(s) with a corpus: ${skills.join(", ") || "(none)"}`);

  // Enumerate every (skill, axis) pair the corpus actually scores, loading each
  // skill's judgements once.
  const pairs: Pair[] = [];
  for (const skill of skills) {
    const records = deps.store.listJudgements({ skill, provider: opts.provider, promptVersion: JUDGE_PROMPT_VERSION });
    for (const axis of NOISE_AXES.filter((a) => records.some((r) => r.scores[a] !== null))) {
      pairs.push({ skill, axis, records, lastAttemptAt: deps.improverStore.get(skill, axis)?.lastAttemptAt ?? "" });
    }
  }

  // Monitor every pair first — free (no codex): auto-revert a ship whose live
  // trend fell, settle a held one. "watching"/"reverted" pairs skip authoring.
  const eligible: Pair[] = [];
  for (const p of pairs) {
    const action = await monitorPending(deps, opts, p.skill, p.axis, p.records, new Date().toISOString());
    if (action === "author" || action === "kept") eligible.push(p);
  }

  // Author for at most maxCyclesPerTick pairs — the codex-heavy step — picking
  // the least-recently-attempted first so coverage rotates and no single tickImprover
  // can spike the shared quota.
  eligible.sort((a, b) => a.lastAttemptAt.localeCompare(b.lastAttemptAt));
  const toRun = eligible.slice(0, Math.max(0, opts.maxCyclesPerTick));
  log(`[improve-worker] ${eligible.length} pair(s) eligible to author; running ${toRun.length} this tickImprover (cap ${opts.maxCyclesPerTick})`);

  for (const p of toRun) {
    const nowISO = new Date().toISOString();
    try {
      const result = await runImproveCycle(deps, {
        skill: p.skill,
        axis: p.axis,
        provider: opts.provider,
        cluster: opts.cluster,
        holdout: opts.holdout,
        samples: opts.samples,
        absMax: opts.absMax,
        bar: opts.bar,
        holdoutMin: opts.holdoutMin,
        recentDays: opts.recentDays,
        k: opts.k,
        apply: opts.apply,
        maxAttempts: opts.maxAttempts,
        budget: opts.budget,
        guardFaithfulness: opts.guardFaithfulness,
        now: Date.now(),
      });
      log(`[${p.skill}/${p.axis}] cycle → ${result.outcome}${result.mode ? ` (mode: ${result.mode})` : ""}`);
      recordCycle(deps, p.skill, p.axis, result, nowISO);
      deps.audit?.({
        at: nowISO,
        kind: "cycle",
        skill: p.skill,
        axis: p.axis,
        outcome: result.outcome,
        mode: result.mode ?? null,
        lesson: result.lesson ?? null,
        reasons: result.reasons ?? null,
      });
    } catch (err) {
      // A cycle that throws (a flaky generator/judge call, a bad trace) must NOT
      // abort the tickImprover or starve the round-robin: record the attempt (so the
      // pair moves to the back of the queue) and audit the failure so a week of
      // logs shows it too.
      const msg = err instanceof Error ? err.message : String(err);
      log(`[${p.skill}/${p.axis}] cycle FAILED: ${msg}`);
      recordCycle(deps, p.skill, p.axis, { outcome: "error" }, nowISO);
      deps.audit?.({ at: nowISO, kind: "cycle", skill: p.skill, axis: p.axis, outcome: "error", error: msg });
    }
  }
}

export async function runImproveWorker(deps: ImproveWorkerDeps, opts: ImproveWorkerOpts): Promise<void> {
  const log = deps.log ?? (() => {});
  log(
    `[improve-worker] provider=${opts.provider} apply=${opts.apply} interval=${opts.pollIntervalMs}ms ` +
      `cap=${opts.maxCyclesPerTick}/tickImprover maxAttempts=${opts.maxAttempts} guardFaith=${opts.guardFaithfulness} ` +
      `recent=${opts.recentDays}d minMonitorN=${opts.minMonitorN} budget=${opts.budget} once=${opts.once}`,
  );
  if (!opts.apply) log("[improve-worker] SHADOW MODE (IMPROVE_APPLY not set) — proposes + gates but never ships.");

  do {
    try {
      await tickImprover(deps, opts);
    } catch (err) {
      log(`[improve-worker] tickImprover failed: ${err instanceof Error ? err.message : String(err)}`);
    }
    if (opts.once) break;
    await sleep(opts.pollIntervalMs);
  } while (true);
}

// ═══════════════════════════════════════════════════════════════════
// Judge worker
// ═══════════════════════════════════════════════════════════════════

export interface JudgeWorkerDeps {
  source: TraceSource;
  writeScores: ScoreWriter;
}

export interface JudgeWorkerOpts {
  provider: JudgeProvider;
  pollIntervalMs: number;
  recentLimit: number;
  dryRun: boolean;
  once: boolean;
}

export function judgeWorkerOptsFromEnv(): JudgeWorkerOpts {
  const provider = process.env.JUDGE_PROVIDER === "openai" ? "openai" : "codex";
  return {
    provider,
    pollIntervalMs: Number(process.env.JUDGE_POLL_INTERVAL_MS ?? 60_000),
    recentLimit: Number(process.env.JUDGE_RECENT_LIMIT ?? 20),
    dryRun: !boolEnv("JUDGE_WRITE_SCORES", false),
    once: boolEnv("JUDGE_ONCE", false),
  };
}

async function processTrace(
  traceId: string,
  deps: JudgeWorkerDeps,
  opts: JudgeWorkerOpts,
): Promise<void> {
  const { nodes } = await assembleNodeMaterials(deps.source, traceId);
  if (nodes.length === 0) {
    // No generative node to score (e.g. a pure-agentic fallback run whose only
    // generations live inside an AGENT black box). Nothing to persist; the
    // trace stays unjudged but is cheap to re-skip (no LLM call).
    console.log(`[judge-worker] ${traceId} has no judgeable nodes — skipping`);
    return;
  }

  const backend = createJudgeBackend(opts.provider);
  console.log(`[judge-worker] judging ${traceId} provider=${opts.provider} nodes=${nodes.length}`);

  // Judge ALL nodes first. If any throws (e.g. codex usage limit), we persist
  // NOTHING and the trace reappears next tickJudge — the whole-trace auto-retry the
  // single-judgement worker had, preserved per-node. Sequential keeps codex
  // within the shared ChatGPT quota.
  const judged: Array<{ node: NodeMaterial; verdict: NodeJudgement }> = [];
  for (const node of nodes) {
    const verdict = await judgeNode(backend, node);
    judged.push({ node, verdict });
  }

  for (const { node, verdict } of judged) {
    await deps.writeScores.write(verdict.scorecard, verdict.faithfulness, {
      traceId,
      observationId: node.observationId,
      nodeKind: node.kind,
      skill: node.skill,
      provider: opts.provider,
      promptVersion: JUDGE_PROMPT_VERSION,
      dryRun: opts.dryRun,
    });
  }
}

async function tickJudge(deps: JudgeWorkerDeps, opts: JudgeWorkerOpts): Promise<void> {
  // The local source returns only COMPLETE runs (written on trace.end()) that
  // lack ANY judgement for this (provider, version) — so no age filter and no
  // separate dedup are needed. A trace that throws (e.g. Codex usage limit)
  // writes no rows and reappears next tickJudge: auto-retry.
  const traces = await deps.source.recentTraces(opts.recentLimit, {
    provider: opts.provider,
    promptVersion: JUDGE_PROMPT_VERSION,
  });
  for (const trace of traces) {
    try {
      await processTrace(trace.id, deps, opts);
    } catch (err) {
      const msg = err instanceof Error ? err.message : String(err);
      console.error(`[judge-worker] ${trace.id} failed: ${msg}`);
    }
  }
}

export async function runJudgeWorker(deps: JudgeWorkerDeps, opts: JudgeWorkerOpts): Promise<void> {
  console.log(
    `[judge-worker] start provider=${opts.provider} recent=${opts.recentLimit} ` +
      `interval=${opts.pollIntervalMs}ms dryRun=${opts.dryRun}`,
  );
  let stopping = false;
  process.on("SIGINT", () => {
    stopping = true;
  });
  process.on("SIGTERM", () => {
    stopping = true;
  });

  do {
    await tickJudge(deps, opts);
    if (opts.once) break;
    await sleep(opts.pollIntervalMs);
  } while (!stopping);
}
