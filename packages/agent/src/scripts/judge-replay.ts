import "dotenv/config";
import { readFileSync, writeFileSync } from "node:fs";
import OpenAI from "openai";
import type { ChatCompletionMessageParam } from "openai/resources/chat/completions";
import type { ReasoningEffort } from "openai/resources/shared";
import { config as loadEnv } from "dotenv";
import { z } from "zod";
import { fetchTraceById, type Observation } from "./langfuse-api";
import { DEEPSEEK_BASE_URL, GEMINI_BASE_URL, retryOnTransient } from "../providers";
import { createWorkflowSchema, parseWorkflow } from "../workflow/dsl";

// A/B replay over a captured trace. Both tests reduce to ONE pattern: take a
// generation's recorded input (which already pins everything but the model —
// the compiler's input holds the signal, the composer's input holds the
// already-rendered tool results) and replay it under a different model.
//
//   Test A (plan):    replay the `planner` generation     -> a Workflow JSON
//   Test B (compose): replay the `llm_compose` generation -> a digest text
//
//   tsx judge-replay.ts <traceId>                            # extract + inspect
//   tsx judge-replay.ts <traceId> --plan <model> [--thinking [level]] [--planner-file <path>]
//   tsx judge-replay.ts <traceId> --compose <model> [--thinking [level]] [--judge]
//
// --planner-file swaps the planner-skill body inside the recorded system
// message for the given on-disk file (the <tools>/<skills> reference block
// stays as recorded) — an A/B of a planner.md edit over a frozen signal.
// --thinking takes an optional level (low/medium/high; bare flag = high) —
// pass `low` to mirror the production compiler preset.
//
// With --judge, the original (A) and each B sample go to a pairwise judge
// (gpt-5.4) over the SAME frozen candidates — both in one call (one salience
// bar), each pair judged in BOTH orders and a winner kept only if it survives
// the swap (a verdict that flips with position is the judge's position bias,
// counted as a tie). JUDGE_PAIRS caps how many samples get the (costly) swap.

loadEnv({ path: ".env.agent" });

// ─── providers ───────────────────────────────────────────────────────

const openai = new OpenAI({ apiKey: process.env.OPENAI_API_KEY });
const deepseek = new OpenAI({
  apiKey: process.env.DEEPSEEK_API_KEY,
  baseURL: DEEPSEEK_BASE_URL,
});
// Gemini exposes an OpenAI-compatible endpoint — same SDK, different baseURL/key.
const gemini = new OpenAI({
  apiKey: process.env.GEMINI_API_KEY,
  baseURL: GEMINI_BASE_URL,
});
function clientFor(model: string): OpenAI {
  if (model.startsWith("deepseek")) return deepseek;
  if (model.startsWith("gemini")) return gemini;
  return openai;
}

// ─── helpers ─────────────────────────────────────────────────────────

function truncate(s: string, n: number): string {
  return s.length <= n ? s : `${s.slice(0, n)}… (+${s.length - n} chars)`;
}

function argFlag(name: string): string | undefined {
  const i = process.argv.indexOf(name);
  return i >= 0 ? process.argv[i + 1] : undefined;
}

interface ChatMessage {
  role: string;
  content: unknown;
}

function asMessages(input: unknown): ChatMessage[] {
  if (!Array.isArray(input)) return [];
  return input.filter(
    (m): m is ChatMessage => typeof m === "object" && m !== null && "role" in m,
  );
}

function messageText(content: unknown): string {
  return typeof content === "string" ? content : JSON.stringify(content);
}

// Rebuild ChatCompletionMessageParam[] from the recorded input — no cast: we
// reconstruct each message from its role + stringified content. compose/plan
// inputs are only system/user (no tool messages).
function toChatMessages(input: unknown): ChatCompletionMessageParam[] {
  return asMessages(input).map((m) => {
    const content = messageText(m.content);
    if (m.role === "system") return { role: "system", content };
    if (m.role === "assistant") return { role: "assistant", content };
    return { role: "user", content };
  });
}

// The last generation whose name starts with `prefix`. For the compiler that's
// the last `attempt-N` (the successful one after retries); compose is single.
function findGeneration(obs: Observation[], prefix: string): Observation | null {
  const matches = obs
    .filter((o) => o.type === "GENERATION" && o.name.startsWith(prefix))
    .sort((a, b) => new Date(a.startTime).getTime() - new Date(b.startTime).getTime());
  return matches.length > 0 ? matches[matches.length - 1]! : null;
}

function outputText(gen: Observation): string {
  return typeof gen.output === "string" ? gen.output : JSON.stringify(gen.output);
}

// Retry 429 (rate limit) and 5xx with exponential backoff — the shared
// providers/retry helper, tuned wider here: free-tier Gemini rate-limits
// hard (5 parallel thinking calls trip its per-minute quota), and the
// gpt-5.4 judge blows the OpenAI per-minute token budget when several
// swap-pairs send ~160k-char candidate sets at once. The backoff
// (3s..48s) outlives a one-minute window.
function withRetry<T>(fn: () => Promise<T>): Promise<T> {
  return retryOnTransient(fn, { maxRetries: 5, baseDelayMs: 3000 });
}

async function replayGeneration(
  messages: ChatCompletionMessageParam[],
  model: string,
  jsonMode: boolean,
  reasoningEffort?: ReasoningEffort,
): Promise<string> {
  const client = clientFor(model);
  const res = await withRetry(() =>
    client.chat.completions.create({
      model,
      messages,
      ...(jsonMode ? { response_format: { type: "json_object" } } : {}),
      ...(reasoningEffort ? { reasoning_effort: reasoningEffort } : {}),
    }),
  );
  return res.choices[0]?.message.content ?? "";
}

function printWorkflow(label: string, model: string | null, json: string): void {
  console.log(`\n=== ${label} (${model ?? "—"}) ===`);
  let wf: unknown;
  try {
    wf = JSON.parse(json);
  } catch (e) {
    console.log(`(invalid JSON: ${(e as Error).message})`);
    console.log(truncate(json, 400));
    return;
  }
  const steps = (wf as { steps?: unknown[] }).steps ?? [];
  console.log(`${steps.length} top-level steps:`);
  printSteps(steps, "  ");
}

function printSteps(steps: unknown[], indent: string): void {
  for (const s of steps) {
    if (typeof s !== "object" || s === null) continue;
    const step = s as Record<string, unknown>;
    const parts = [String(step.kind ?? "?")];
    if (step.tool) parts.push(`${String(step.tool)}(${JSON.stringify(step.args ?? {})})`);
    if (step.skill) parts.push(`skill=${String(step.skill)} preset=${String(step.preset)}`);
    console.log(`${indent}- ${parts.join(" ")}`);
    if (Array.isArray(step.steps)) printSteps(step.steps, `${indent}  `);
  }
}

// ─── modes ───────────────────────────────────────────────────────────

function inspect(observations: Observation[]): void {
  for (const [label, prefix] of [
    ["PLANNER (Test A input)", "attempt-"],
    ["COMPOSE (Test B input)", "llm_compose"],
  ] as const) {
    const gen = findGeneration(observations, prefix);
    console.log(`\n=== ${label} ===`);
    if (!gen) {
      console.log("(not found)");
      continue;
    }
    const msgs = asMessages(gen.input);
    console.log(`name=${gen.name}  model=${gen.model ?? "—"}  messages=${msgs.length}`);
    for (const m of msgs) {
      const text = messageText(m.content);
      console.log(`  • ${m.role} (${text.length} chars): ${truncate(text, 180).replace(/\n/g, " ")}`);
    }
    console.log(`output (${outputText(gen).length} chars): ${truncate(outputText(gen), 300)}`);
  }
}

// Always sample N times — one lucky first shot is a poor metric in a
// non-deterministic system. Each sample is an INDEPENDENT call on the same
// input (no retry-feedback between them), so the spread reflects the model's
// raw reliability at building a workflow, not the retry loop's.
const SAMPLES = 5;

// Lite structural validity — JSON-parses + has a steps[] where every step
// carries a `kind`. Catches the format failures (markdown wrapper, trailing
// text) without needing the full tool/skill enum schema.
function validateWorkflowLite(json: string): { ok: boolean; error?: string; steps?: number } {
  let wf: unknown;
  try {
    wf = JSON.parse(json);
  } catch (e) {
    return { ok: false, error: `JSON: ${(e as Error).message}` };
  }
  if (typeof wf !== "object" || wf === null) return { ok: false, error: "not an object" };
  const steps = (wf as Record<string, unknown>).steps;
  if (!Array.isArray(steps)) return { ok: false, error: "missing steps[]" };
  for (const s of steps) {
    if (typeof s !== "object" || s === null || !("kind" in s)) {
      return { ok: false, error: "a step has no kind" };
    }
  }
  return { ok: true, steps: steps.length };
}

// ─── full-schema validation from the recorded prompt ─────────────────
// The recorded system message carries the exact <tools>/<skills> reference
// the compiler saw, so we can rebuild the SAME production Zod schema
// (tool/skill enums included) and validate samples for real — not just
// "parses and has kinds".

function extractBlock(text: string, tag: string): string | null {
  const m = new RegExp(`<${tag}>\\n([\\s\\S]*?)\\n</${tag}>`).exec(text);
  return m?.[1] ?? null;
}

function schemaFromRecordedPrompt(
  messages: ChatCompletionMessageParam[],
): z.ZodTypeAny | null {
  const sys = messages.find((m) => m.role === "system");
  if (!sys || typeof sys.content !== "string") return null;
  const toolsBlock = extractBlock(sys.content, "tools");
  const skillsBlock = extractBlock(sys.content, "skills");
  if (!toolsBlock || !skillsBlock) return null;
  const knownTools = [...toolsBlock.matchAll(/^- ([a-zA-Z0-9_]+)\(/gm)].map((m) => m[1]!);
  const knownSkills = [...skillsBlock.matchAll(/^- (\S+)$/gm)].map((m) => m[1]!);
  if (knownTools.length === 0 || knownSkills.length === 0) return null;
  return createWorkflowSchema({ knownTools, knownSkills }).WorkflowSchema;
}

interface SampleVerdict {
  ok: boolean;
  steps?: number;
  errors: string[];
}

function validateSample(json: string, schema: z.ZodTypeAny | null): SampleVerdict {
  const lite = validateWorkflowLite(json);
  if (!lite.ok) return { ok: false, errors: [lite.error ?? "invalid"] };
  if (!schema) return { ok: true, steps: lite.steps, errors: [] };
  const parsed = parseWorkflow(JSON.parse(json), schema);
  return parsed.ok
    ? { ok: true, steps: lite.steps, errors: [] }
    : { ok: false, steps: lite.steps, errors: parsed.errors };
}

// First balanced JSON object from a reply with trailing garbage (the
// gpt-5.4-mini duplicate-object pathology emits `{…}\n{…}`). Mirrors what a
// production-side salvage would do: JSON.parse pinpoints the offset where
// extra content starts; everything before it is the complete first object.
function salvageFirstJson(text: string): string | null {
  try {
    JSON.parse(text);
    return null; // parsed whole — nothing to salvage
  } catch (e) {
    const m = (e as Error).message.match(/position (\d+)/);
    if (!m) return null;
    const head = text.slice(0, Number(m[1]));
    try {
      JSON.parse(head);
      return head;
    } catch {
      return null;
    }
  }
}

// Swap the planner-skill body in the recorded system message for the given
// on-disk file, keeping the recorded <tools>/<skills> reference intact.
// skills.ts strips frontmatter before the body reaches the compiler —
// mirrored here so the swapped prompt matches what production would send.
function swapPlannerBody(
  messages: ChatCompletionMessageParam[],
  plannerPath: string,
): void {
  const sys = messages.find((m) => m.role === "system");
  if (!sys || typeof sys.content !== "string") {
    throw new Error("no system message in recorded input to swap");
  }
  const idx = sys.content.indexOf("<tools>");
  if (idx < 0) throw new Error("recorded system message has no <tools> block");
  let body = readFileSync(plannerPath, "utf8");
  if (body.startsWith("---\n")) {
    const end = body.indexOf("\n---\n");
    if (end >= 0) body = body.slice(end + 5);
  }
  sys.content = `${body.trim()}\n\n${sys.content.slice(idx)}`;
}

// Test A: replay the planner's input under model B, SAMPLES times. A is the
// original output (one reference); B is the distribution of fresh samples.
// With plannerFile, B runs on the swapped-in prompt (A stays the recorded
// run for reference).
async function testPlan(
  observations: Observation[],
  modelB: string,
  reasoningEffort?: ReasoningEffort,
  plannerFile?: string,
  dumpDir?: string,
): Promise<void> {
  const planner = findGeneration(observations, "attempt-");
  if (!planner) throw new Error("no planner generation in trace");
  const messages = toChatMessages(planner.input);
  // Schema enums come from the RECORDED reference block — identical before
  // and after the swap, since swapPlannerBody preserves it.
  const schema = schemaFromRecordedPrompt(messages);
  if (!schema) console.log("(no <tools>/<skills> block found — falling back to lite validation)");

  if (plannerFile) {
    const before = messages.find((m) => m.role === "system");
    const beforeLen = typeof before?.content === "string" ? before.content.length : 0;
    swapPlannerBody(messages, plannerFile);
    const afterLen = typeof before?.content === "string" ? before.content.length : 0;
    console.log(`planner body swapped in from ${plannerFile} (system ${beforeLen} → ${afterLen} chars)`);
  }

  printWorkflow("PLAN A — original (recorded prompt)", planner.model, outputText(planner));

  const thinkLabel = reasoningEffort ? ` (thinking=${reasoningEffort})` : "";
  const promptLabel = plannerFile ? `, prompt=${plannerFile}` : ", prompt=recorded";
  console.log(`\n=== PLAN B — ${modelB}${thinkLabel}${promptLabel}, ${SAMPLES} independent samples ===`);
  const samples = await Promise.all(
    Array.from({ length: SAMPLES }, () => replayGeneration(messages, modelB, true, reasoningEffort)),
  );
  const withSalvage = process.argv.includes("--salvage");
  // Simulate the production compile retry loop: on an invalid reply, append
  // it + the validation errors as feedback (same wording as compile.ts
  // pushRetryFeedback) and re-ask, up to N extra attempts per sample.
  const retries = Number(argFlag("--retries") ?? "0") || 0;
  let valid = 0;
  let salvaged = 0;
  let attemptsTotal = 0;

  for (let i = 0; i < samples.length; i++) {
    let out = samples[i]!;
    if (dumpDir) writeFileSync(`${dumpDir}/sample-${i + 1}.json`, out);
    const convo = [...messages];
    let v: SampleVerdict = { ok: false, errors: [] };
    let wasSalvaged = false;
    let attempt = 0;
    for (attempt = 1; attempt <= 1 + retries; attempt++) {
      let candidate = out;
      let salvagedNow = false;
      if (withSalvage) {
        const head = salvageFirstJson(candidate);
        if (head !== null) {
          candidate = head;
          salvagedNow = true;
        }
      }
      v = validateSample(candidate, schema);
      if (v.ok) {
        out = candidate;
        wasSalvaged = salvagedNow;
        break;
      }
      if (attempt > retries) break;
      convo.push({ role: "assistant", content: out });
      convo.push({
        role: "user",
        content: [
          "Your previous workflow failed validation. Errors:",
          ...v.errors.map((e) => `  - ${e}`),
          "",
          "Emit a corrected workflow. Return ONLY the JSON, no markdown wrapper.",
        ].join("\n"),
      });
      // Sequential by design — a retry depends on the previous reply.
      out = await replayGeneration(convo, modelB, true, reasoningEffort);
    }
    if (wasSalvaged) salvaged += 1;
    if (v.ok) valid += 1;
    attemptsTotal += Math.min(attempt, 1 + retries);
    const tags = [
      wasSalvaged ? "salvaged" : "",
      attempt > 1 ? `attempts=${Math.min(attempt, 1 + retries)}` : "",
    ]
      .filter(Boolean)
      .join(", ");
    console.log(
      `\n[sample ${i + 1}] ${v.ok ? `✓ valid (${v.steps} steps)` : `✗ invalid${v.steps !== undefined ? ` (${v.steps} steps)` : ""}`}${tags ? ` [${tags}]` : ""}`,
    );
    for (const e of v.errors.slice(0, 4)) console.log(`    ! ${e}`);
    const lite = validateWorkflowLite(out);
    if (lite.ok) {
      printSteps((JSON.parse(out) as { steps: unknown[] }).steps, "  ");
    } else {
      console.log(`  ${truncate(out, 220).replace(/\n/g, " ")}`);
    }
  }
  const salvageNote = withSalvage ? `, ${salvaged} salvaged` : "";
  const retryNote = retries > 0 ? `, ${(attemptsTotal / samples.length).toFixed(1)} avg attempts` : "";
  console.log(
    `\nB valid: ${valid}/${SAMPLES}${schema ? " (full production schema)" : " (lite)"}${salvageNote}${retryNote}`,
  );
}

// Test B: replay the composer's input under model B, SAMPLES times. The input
// already holds the frozen tool results (the rendered candidate posts), so this
// is a pure compose-model A/B. Output is prose — no JSON validation; with
// `judge` A and each B sample are compared by the swap-debiased judge (below).
async function testCompose(
  observations: Observation[],
  modelB: string,
  reasoningEffort?: ReasoningEffort,
  withJudge = false,
): Promise<void> {
  const compose = findGeneration(observations, "llm_compose");
  if (!compose) throw new Error("no llm_compose generation in trace");
  const messages = toChatMessages(compose.input);
  const original = outputText(compose);

  console.log(`\n=== COMPOSE A — original (${compose.model ?? "—"}) ===`);
  console.log(original);

  const thinkLabel = reasoningEffort ? ` (thinking=${reasoningEffort})` : "";
  console.log(`\n=== COMPOSE B — ${modelB}${thinkLabel}, ${SAMPLES} independent samples ===`);
  const samples = await Promise.all(
    Array.from({ length: SAMPLES }, () => replayGeneration(messages, modelB, false, reasoningEffort)),
  );
  samples.forEach((out, i) => {
    console.log(`\n[sample ${i + 1}] (${out.length} chars)`);
    console.log(out);
  });

  if (withJudge) await judgeCompose(compose, original, samples, modelB);
}

// ─── pairwise judge with swap-debias (stage 3) ───────────────────────
// Both digests go to the judge in ONE call, so it applies ONE salience bar to
// the pair — fixing the flaw pointwise had (critiquing each digest alone let the
// bar drift: the same ЗАЭС miss got penalized for A but waved through for B).
// The price of one-call pairwise is position bias: with a small real gap the
// judge drifts to whichever digest is listed first. We cancel it by judging each
// pair TWICE with the order swapped, and keeping a winner ONLY when it wins in
// BOTH layouts; a verdict that flips with position IS the bias — recorded as tie.
//
// query_formulation is out of scope — both digests share one frozen input, so
// orchestration is identical; only the three composer axes differ. Judge is
// gpt-5.4, a different family from both generators (deepseek A, gpt-5.4-mini B).

const JUDGE_MODEL = "gpt-5.4";

// How many B samples to swap-judge. Each pair costs TWO gpt-5.4 calls over the
// full candidate set (~150k chars), so this is the expensive knob — generation
// stays at SAMPLES (cheap mini calls we still eyeball), judging is capped here.
const JUDGE_PAIRS = 3;

const VerdictSchema = z.object({
  winner: z.enum(["digest_1", "digest_2", "tie"]),
  why: z.string(),
});
const PairwiseSchema = z.object({
  coverage: VerdictSchema,
  faithfulness: VerdictSchema.extend({
    digest_1_issues: z.array(z.string()),
    digest_2_issues: z.array(z.string()),
  }),
  composition: VerdictSchema,
  overall: VerdictSchema,
});
type Pairwise = z.infer<typeof PairwiseSchema>;

// JSON Schema mirror for OpenAI strict structured output, kept in sync with the
// zod schema above by hand (small enough that a generator isn't worth it).
const verdictJsonSchema = () => ({
  type: "object",
  properties: {
    winner: { type: "string", enum: ["digest_1", "digest_2", "tie"] },
    why: { type: "string" },
  },
  required: ["winner", "why"],
  additionalProperties: false,
});
const PAIRWISE_RESPONSE_SCHEMA = {
  type: "object",
  properties: {
    coverage: verdictJsonSchema(),
    faithfulness: {
      type: "object",
      properties: {
        winner: { type: "string", enum: ["digest_1", "digest_2", "tie"] },
        why: { type: "string" },
        digest_1_issues: { type: "array", items: { type: "string" } },
        digest_2_issues: { type: "array", items: { type: "string" } },
      },
      required: ["winner", "why", "digest_1_issues", "digest_2_issues"],
      additionalProperties: false,
    },
    composition: verdictJsonSchema(),
    overall: verdictJsonSchema(),
  },
  required: ["coverage", "faithfulness", "composition", "overall"],
  additionalProperties: false,
};

const PAIRWISE_SYSTEM = `You are a rigorous PAIRWISE evaluation judge for an AI news-digest composer.

Context — the agent runs in two stages: an ORCHESTRATOR gathers candidate posts (RAG + chat history), then a COMPOSER skill filters them and writes the final digest. BOTH digests below were written by COMPOSER models from the EXACT SAME candidates and the SAME contract — orchestration was identical and is NOT under test. Judge ONLY composition quality, head to head, applying ONE consistent salience bar to both digests.

You receive:
- COMPOSER_CONTRACT — how candidates must be filtered and the digest composed (format, thresholds, tone, no-fabrication).
- CANDIDATES — every post both composers saw. This is the ground truth (R) for coverage and faithfulness; it also contains the bot's previous digests (for dedup), which are context, not new material — their absence is never a coverage loss.
- DIGEST_1 and DIGEST_2 — two competing outputs. The numbering is RANDOM and carries no signal about quality or origin; do not assume 1 precedes or beats 2.

Compare on three axes, each a winner of {digest_1, digest_2, tie} with a one-sentence why citing specific items:
- coverage — which digest better included the salient, contract-fitting events from CANDIDATES and dropped the noise. Use the SAME bar for both: if a dropped event (a power restoration, a cross-border strike, a named operation) counts against one digest, it counts against the other too. Dropping a major event (casualties, strikes) is a serious loss.
- composition — which better follows the contract's format, dedup, category placement, tone, length and threshold rules.
- faithfulness — which has fewer fabricated/unsupported specifics (numbers, dates, names) relative to CANDIDATES. List each side's unsupported claims in digest_1_issues / digest_2_issues; fewer/less-severe wins. CRITICAL: a claim that IS present in CANDIDATES is faithful even if it contradicts your outside knowledge — verify against the candidate text, never against what you believe is true in the real world.

Then an overall winner.

Rules:
- tie is a legitimate, encouraged verdict when the two are genuinely comparable on an axis — do not invent a difference to break it.
- Reward neither length nor fluency. A tighter correct digest beats a longer verbose one.
- Never penalize a digest for omitting something that is not in CANDIDATES, and never credit a fabrication.`;

function buildPairwisePrompt(
  contract: string,
  candidates: string,
  digest1: string,
  digest2: string,
): string {
  return `<composer_contract>
${contract}
</composer_contract>

<candidates>
${candidates}
</candidates>

<digest_1>
${digest1}
</digest_1>

<digest_2>
${digest2}
</digest_2>

Both digests were composed from the SAME candidates above by two different models; orchestration was identical and is not under test. Compare them per axis with one consistent salience bar, and return JSON matching the schema.`;
}

async function judgePair(
  contract: string,
  candidates: string,
  digest1: string,
  digest2: string,
): Promise<Pairwise> {
  const res = await withRetry(() =>
    openai.chat.completions.create({
      model: JUDGE_MODEL,
      messages: [
        { role: "system", content: PAIRWISE_SYSTEM },
        { role: "user", content: buildPairwisePrompt(contract, candidates, digest1, digest2) },
      ],
      response_format: {
        type: "json_schema",
        json_schema: { name: "pairwise", strict: true, schema: PAIRWISE_RESPONSE_SCHEMA },
      },
    }),
  );
  const content = res.choices[0]?.message.content;
  if (!content) throw new Error("pairwise judge returned empty content");
  return PairwiseSchema.parse(JSON.parse(content));
}

type AB = "A" | "B" | "tie";

// digest_1/digest_2 are page positions; map to A/B for THIS layout (flipped =
// the B,A layout, where digest_1 is B).
function resolveWinner(winner: "digest_1" | "digest_2" | "tie", flipped: boolean): AB {
  if (winner === "tie") return "tie";
  const isA = flipped ? winner === "digest_2" : winner === "digest_1";
  return isA ? "A" : "B";
}

// Keep a winner only if it holds in BOTH layouts; a verdict that flips with
// position (or is a tie in either layout) collapses to tie — that flip IS the
// position bias we set out to cancel.
function reconcile(w1: AB, w2: AB): AB {
  return w1 === w2 && w1 !== "tie" ? w1 : "tie";
}

const AXES = ["coverage", "faithfulness", "composition", "overall"] as const;
type Axis = (typeof AXES)[number];

async function judgeCompose(
  compose: Observation,
  original: string,
  samples: string[],
  modelB: string,
): Promise<void> {
  const msgs = asMessages(compose.input);
  const contract = msgs
    .filter((m) => m.role === "system")
    .map((m) => messageText(m.content))
    .join("\n\n");
  const candidates = msgs
    .filter((m) => m.role !== "system")
    .map((m) => messageText(m.content))
    .join("\n\n");

  const modelA = compose.model ?? "original";
  const judged = samples.slice(0, JUDGE_PAIRS);
  console.log(`\n${"═".repeat(72)}`);
  console.log(`SWAP-PAIRWISE JUDGE (${JUDGE_MODEL}) — A=${modelA}  vs  B=${modelB}`);
  console.log(
    `${candidates.length} chars of candidates · ${judged.length} pairs × 2 layouts · ` +
      `winner counts only if it holds in BOTH layouts (else tie)\n`,
  );

  // Each pair judged in BOTH layouts — (A,B) and the swap (B,A) — all in
  // parallel. resolveWinner maps each layout's digest_1/digest_2 back to A/B.
  const results = await Promise.all(
    judged.map(async (b, i) => {
      const [ab, ba] = await Promise.all([
        judgePair(contract, candidates, original, b),
        judgePair(contract, candidates, b, original),
      ]);
      return { i, ab, ba };
    }),
  );

  const tally: Record<Axis, { A: number; B: number; tie: number }> = {
    coverage: { A: 0, B: 0, tie: 0 },
    faithfulness: { A: 0, B: 0, tie: 0 },
    composition: { A: 0, B: 0, tie: 0 },
    overall: { A: 0, B: 0, tie: 0 },
  };

  for (const { i, ab, ba } of results) {
    console.log(`[sample ${i + 1}]  layout A,B  vs  swap B,A`);
    for (const axis of AXES) {
      const w1 = resolveWinner(ab[axis].winner, false);
      const w2 = resolveWinner(ba[axis].winner, true);
      const final = reconcile(w1, w2);
      tally[axis][final] += 1;
      const flip = w1 !== w2 ? "  ⟂ flipped→bias" : "";
      console.log(`  ${axis.padEnd(13)} AB:${w1.padEnd(3)} BA:${w2.padEnd(3)} ⟹ ${final}${flip}`);
    }
    // Faithfulness defect lists from the A,B layout (digest_1=A, digest_2=B).
    const aIssues = ab.faithfulness.digest_1_issues;
    const bIssues = ab.faithfulness.digest_2_issues;
    if (aIssues.length) console.log(`    A unsupported: ${aIssues.join(" · ")}`);
    if (bIssues.length) console.log(`    B unsupported: ${bIssues.join(" · ")}`);
    console.log(`    coverage why (A,B): ${ab.coverage.why}`);
    console.log();
  }

  console.log(`── tally over ${judged.length} swap-pairs · A=${modelA}, B=${modelB} ──`);
  for (const axis of AXES) {
    const t = tally[axis];
    console.log(`  ${axis.padEnd(13)} A:${t.A}  B:${t.B}  tie:${t.tie}`);
  }
}

// `--thinking` with an optional level. Bare flag keeps the historical
// meaning (high); `--thinking low` mirrors the production compiler preset.
function parseThinking(): ReasoningEffort | undefined {
  const i = process.argv.indexOf("--thinking");
  if (i < 0) return undefined;
  const next = process.argv[i + 1];
  if (next === "low" || next === "medium" || next === "high") {
    return next;
  }
  return "high";
}

async function main(): Promise<void> {
  const traceId = process.argv[2];
  if (!traceId || traceId.startsWith("--")) {
    console.error(
      "usage: tsx judge-replay.ts <traceId> [--plan <model> | --compose <model>] " +
        "[--thinking [low|medium|high]] [--planner-file <path>] [--judge]",
    );
    process.exit(1);
  }
  const { trace, observations } = await fetchTraceById(traceId);
  console.log(`trace ${traceId} · ${trace.name} · tags=[${trace.tags.join(",")}]`);

  const thinking = parseThinking();
  const planModel = argFlag("--plan");
  const composeModel = argFlag("--compose");
  if (planModel !== undefined) {
    await testPlan(
      observations,
      planModel || "gpt-5.4-mini",
      thinking,
      argFlag("--planner-file"),
      argFlag("--dump-dir"),
    );
  } else if (composeModel !== undefined) {
    const withJudge = process.argv.includes("--judge");
    await testCompose(observations, composeModel || "gpt-5.4-mini", thinking, withJudge);
  } else {
    inspect(observations);
  }
}

main().catch((err: unknown) => {
  console.error(err);
  process.exit(1);
});
