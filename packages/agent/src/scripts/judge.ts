import "dotenv/config";
import OpenAI from "openai";
import { z } from "zod";
import { config as loadEnv } from "dotenv";
import { fetchRecentTraces, fetchTraceById, type Observation, type Trace } from "./langfuse-api";
import { readSkillRaw } from "../skills";

// LANGFUSE_* live in .env; OPENAI_API_KEY (the judge model's key) lives in
// .env.agent. Layer .env.agent on top — dotenv doesn't override what .env
// already set, it only fills in the missing OPENAI_API_KEY.
loadEnv({ path: ".env.agent" });

// LLM-as-judge MVP. Reads ONE agent run from Langfuse, hands the judge the raw
// trajectory (RAG queries + retrieved snippets + final output) plus the skill
// contract the agent was meant to follow, and scores fixed axes. No
// normalization yet — we feed the raw flow and see whether the judge copes.
//
// Usage: pnpm judge <traceId>
//
//   - Judge model is a different family from the generators (GPT-5.4 vs the
//     DeepSeek composer), so there's no self-preference on the output.
//   - Contract comes from the skill, not from judge code: a new scenario =
//     a new skill, judge unchanged.
//   - Scores print to stdout for now; Langfuse score ingestion is a later step.

const JUDGE_MODEL = "gpt-5.4";
const JUDGE_PROMPT_VERSION = "v2";

// ─── raw-material assembly ───────────────────────────────────────────

function stringify(x: unknown): string {
  if (x === null || x === undefined) return "null";
  if (typeof x === "string") return x;
  try {
    return JSON.stringify(x, null, 2);
  } catch {
    return String(x);
  }
}

// Which skill composed the output. Workflow path stamps it on the llm_compose /
// llm_agent step observation's metadata.skill; agent-loop path lists it on
// trace.metadata.skills. First non-empty wins.
function findSkill(trace: Trace, observations: Observation[]): string | null {
  for (const o of observations) {
    const skill = o.metadata?.skill;
    if (typeof skill === "string" && skill.length > 0) return skill;
  }
  const skills = trace.metadata?.skills;
  if (Array.isArray(skills) && typeof skills[0] === "string") return skills[0];
  return null;
}

// Flatten the trace into one readable transcript — FULL payloads, no
// truncation (that's the point of reading from the API, not the UI). We drop
// only generation INPUT: it's the model's giant prompt (system + replayed
// history), pure noise for judging. Everything the agent DID — queries,
// retrieved snippets, composed text, tool results — stays verbatim.
function buildTranscript(trace: Trace, observations: Observation[]): string {
  const lines: string[] = [];
  const intent = stringify(trace.input);
  lines.push(
    `# INTENT (trace.input)\n${
      intent === "null"
        ? "(empty — workflow path; the intent for this run is the skill contract below)"
        : intent
    }`,
  );
  lines.push(`# tags: ${trace.tags.join(", ")}`);

  const sorted = [...observations].sort(
    (a, b) => new Date(a.startTime).getTime() - new Date(b.startTime).getTime(),
  );
  lines.push(`\n# FLOW (${sorted.length} observations)`);
  for (const o of sorted) {
    // Skip the trace-root span (mirrors the trace itself).
    if (o.parentObservationId === null && o.name === trace.name) continue;
    lines.push(`\n## ${o.name}  {${o.type.toLowerCase()}}`);
    if (o.statusMessage) lines.push(`status: ${o.statusMessage}`);
    if (o.type === "GENERATION" || o.type === "EMBEDDING") {
      lines.push(`model: ${o.model ?? "—"}`);
      lines.push(`output: ${stringify(o.output)}`);
    } else {
      if (o.input !== null && o.input !== undefined) lines.push(`input: ${stringify(o.input)}`);
      if (o.output !== null && o.output !== undefined) lines.push(`output: ${stringify(o.output)}`);
    }
  }
  lines.push(`\n# FINAL OUTPUT (trace.output)\n${stringify(trace.output)}`);
  return lines.join("\n");
}

// ─── judge ───────────────────────────────────────────────────────────

const AxisResultSchema = z.object({
  axis: z.enum(["coverage", "query_formulation", "composition"]),
  applicable: z.boolean(),
  // 0..1, or null when the axis doesn't apply to this run.
  score: z.number().nullable(),
  label: z.enum(["fail", "weak", "ok", "strong", "n/a"]),
  rationale: z.string(),
  evidence: z.string(),
});
const ScorecardSchema = z.object({
  axes: z.array(AxisResultSchema),
  overall_note: z.string(),
});
type Scorecard = z.infer<typeof ScorecardSchema>;

// JSON Schema mirror of ScorecardSchema for OpenAI structured output (strict
// mode: every property required, no extras). Kept in sync with the zod schema
// above by hand — small enough that a generator isn't worth it yet.
const RESPONSE_SCHEMA = {
  type: "object",
  properties: {
    axes: {
      type: "array",
      items: {
        type: "object",
        properties: {
          axis: { type: "string", enum: ["coverage", "query_formulation", "composition"] },
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

const SYSTEM_PROMPT = `You are a rigorous evaluation judge for an AI agent that handles each signal in TWO stages. Understanding the split is essential to scoring correctly:

1. ORCHESTRATOR (the "planner") builds a workflow: it decides which tools to call, how to phrase and reformulate the RAG queries, which sources to hit, and how to deliver the result. Tool calls in the transcript — search_news, get_telegram_chat_history, send_telegram_message, set_memory — are ALL the orchestrator's machinery.
2. COMPOSER (the skill, e.g. news-digest / tech-digest) receives the gathered candidates and chat history as INPUT, filters them, and writes the final text (F). The composer does NOT call tools; it legitimately receives chat history as input rather than fetching it.

You score ONE completed run. You did not generate it and have no stake in it.

Inputs:
- ORCHESTRATOR_CONTRACT (planner) — how retrieval should be phrased / reformulated / routed.
- COMPOSER_CONTRACT (the skill) — how candidates should be filtered and the output composed: format, thresholds, tone, no-fabrication.
- TRANSCRIPT — the actual run: the orchestrator's RAG queries (Q) and what came back (R), plus the composer's final text (F).

Score each axis from 0 to 1 (fail < 0.3, weak < 0.5, ok < 0.75, strong >= 0.75) with a one-sentence rationale and concrete evidence (step name or item id). CRITICAL — judge each axis against the RIGHT contract:
- query_formulation -> the ORCHESTRATOR_CONTRACT (phrasing / reformulation / source routing) AND the COMPOSER_CONTRACT's stated interests/topics (what the queries should target). Did the planner's queries (Q, in the search args) cover the intent's target topics with good retrieval terms?
- coverage -> the COMPOSER_CONTRACT. Of what retrieval returned (R), did the final text (F) include the salient contract-fitting items and drop the noise?
- composition -> the COMPOSER_CONTRACT. Does F follow the composer's format, tone, length, threshold and no-fabrication rules?

DECISIVE RULE — never penalize the COMPOSER for ORCHESTRATION. Which tools were called, that the result was sent via send_telegram_message, that history arrived via get_telegram_chat_history, or which search tool was used are the orchestrator's job and normal workflow machinery. They are NEVER a coverage or composition violation. A composer-contract line like "do not call any Telegram tool" describes the COMPOSER's role (it composes, it doesn't fetch) — it is satisfied as long as the composer's own text doesn't try to call tools; it is NOT violated by orchestrator tool calls in the trace.

Other rules:
- Obey the contracts. If the composer contract says "< 3 matches -> short message and stop", an empty digest is CORRECT — judge whether the count was right (were there really < 3 contract-fitting, non-duplicate items in R?), not whether it produced a digest.
- If an axis does not apply to this run (nothing to deduplicate, or an empty output has no facts to verify), set applicable=false, score=null, label="n/a".
- Reward neither length nor fluency. A correct short output beats a verbose wrong one.
- Ground every claim in the TRANSCRIPT. Never invent items that aren't in R.`;

function buildUserPrompt(
  composerSkill: string | null,
  composerContract: string | null,
  orchestratorContract: string | null,
  transcript: string,
): string {
  return `<orchestrator_contract skill="planner">
${orchestratorContract ?? "(planner contract unavailable)"}
</orchestrator_contract>

<composer_contract skill="${composerSkill ?? "unknown"}">
${composerContract ?? "(no composer skill contract found — judge against general digest/answer expectations)"}
</composer_contract>

<transcript>
${transcript}
</transcript>

Score this run. Return JSON matching the schema, with exactly these three axes: coverage, query_formulation, composition.`;
}

async function judge(
  openai: OpenAI,
  composerSkill: string | null,
  composerContract: string | null,
  orchestratorContract: string | null,
  transcript: string,
): Promise<Scorecard> {
  const res = await openai.chat.completions.create({
    model: JUDGE_MODEL,
    messages: [
      { role: "system", content: SYSTEM_PROMPT },
      {
        role: "user",
        content: buildUserPrompt(composerSkill, composerContract, orchestratorContract, transcript),
      },
    ],
    response_format: {
      type: "json_schema",
      json_schema: { name: "scorecard", strict: true, schema: RESPONSE_SCHEMA },
    },
  });
  const content = res.choices[0]?.message.content;
  if (!content) throw new Error("judge returned empty content");
  return ScorecardSchema.parse(JSON.parse(content));
}

// ─── faithfulness sub-judge ──────────────────────────────────────────
// Separate call, claim-decomposition style (RAGAS-ish): extract atomic
// factual claims from F, verify each against R's snippets. A "rate
// faithfulness 0..1" vibe score is unreliable for catching fabricated
// specifics (numbers / dates); per-claim verdicts are not. The composer
// contract calls hallucinated specifics "the most damaging error", so this
// gets its own focused pass rather than folding into the holistic call.

const FaithClaimSchema = z.object({
  claim: z.string(),
  verdict: z.enum(["supported", "partial", "unsupported"]),
  evidence: z.string(),
});
const FaithfulnessSchema = z.object({
  applicable: z.boolean(),
  claims: z.array(FaithClaimSchema),
  // (supported + 0.5*partial) / total; null when no factual claims (quiet day).
  score: z.number().nullable(),
  note: z.string(),
});
type Faithfulness = z.infer<typeof FaithfulnessSchema>;

const FAITH_RESPONSE_SCHEMA = {
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

const FAITH_SYSTEM_PROMPT = `You are a faithfulness checker for an AI-composed news digest or answer. You verify that every factual claim in the FINAL OUTPUT (F) is grounded in the retrieved snippets (R) or the chat history shown in the transcript. Per the composer contract, quoting specifics — numbers, counts, dates, version numbers — not present in a snippet is the single most damaging error class.

Method:
1. Extract ATOMIC factual claims from F. Focus on verifiable specifics: numbers/counts, dates, named entities, concrete events, and comparisons ("up from 63 the day before").
2. For each claim, look for support in R's snippets (and the chat history). Verdict:
   - supported — the claim and its specifics appear in some snippet/item.
   - partial — the gist is backed but a specific (number/date/name) is missing, altered, or aggregated beyond what any single snippet states.
   - unsupported — no item backs it; likely fabricated or editorialized.
3. Cite the supporting (or contradicting) item id as evidence, or "none".

Rules:
- Judge only F's factual content. The digest's own header/date line, category labels, and emojis are not claims.
- A hard number synthesized by aggregating several monitoring/channel posts is at best PARTIAL unless a snippet states that number.
- If F is an empty / "тихий день" message with no factual claims, set applicable=false, score=null, claims=[].
- score = (count(supported) + 0.5 * count(partial)) / total_claims, rounded to 2 decimals.
- Ground every verdict in the transcript; never invent snippet content.`;

function buildFaithUserPrompt(composerContract: string | null, transcript: string): string {
  return `<composer_contract>
${composerContract ?? "(no contract)"}
</composer_contract>

<transcript>
${transcript}
</transcript>

Extract F's atomic factual claims and verify each against R. Return JSON per the schema.`;
}

async function judgeFaithfulness(
  openai: OpenAI,
  composerContract: string | null,
  transcript: string,
): Promise<Faithfulness> {
  const res = await openai.chat.completions.create({
    model: JUDGE_MODEL,
    messages: [
      { role: "system", content: FAITH_SYSTEM_PROMPT },
      { role: "user", content: buildFaithUserPrompt(composerContract, transcript) },
    ],
    response_format: {
      type: "json_schema",
      json_schema: { name: "faithfulness", strict: true, schema: FAITH_RESPONSE_SCHEMA },
    },
  });
  const content = res.choices[0]?.message.content;
  if (!content) throw new Error("faithfulness judge returned empty content");
  return FaithfulnessSchema.parse(JSON.parse(content));
}

// ─── output ──────────────────────────────────────────────────────────

function printScorecard(traceId: string, skillName: string | null, card: Scorecard): void {
  console.log(
    `\n=== JUDGE ${JUDGE_MODEL} (prompt ${JUDGE_PROMPT_VERSION}) · trace ${traceId} · skill ${skillName ?? "—"} ===\n`,
  );
  for (const a of card.axes) {
    const score = a.applicable && a.score !== null ? a.score.toFixed(2) : "n/a";
    console.log(`● ${a.axis}: ${a.label} (${score})`);
    console.log(`  ${a.rationale}`);
    if (a.evidence) console.log(`  ↳ ${a.evidence}`);
    console.log();
  }
  console.log(`overall: ${card.overall_note}\n`);
}

function printFaithfulness(f: Faithfulness): void {
  if (!f.applicable) {
    console.log(`● faithfulness: n/a — ${f.note}\n`);
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
  console.log();
}

async function judgeOne(openai: OpenAI, traceId: string): Promise<void> {
  const { trace, observations } = await fetchTraceById(traceId);
  const skillName = findSkill(trace, observations);
  const composerContract = skillName ? await readSkillRaw(skillName) : null;
  // The planner/orchestrator owns tool choice + query phrasing; load its
  // contract separately so query_formulation is judged against it, not against
  // the composer skill (which never calls tools).
  const orchestratorContract = await readSkillRaw("planner");
  const transcript = buildTranscript(trace, observations);

  console.error(
    `[judge] trace ${traceId} · skill=${skillName ?? "—"} · ${observations.length} obs · ` +
      `transcript ${transcript.length} chars`,
  );

  // Holistic axes and the faithfulness sub-judge are independent calls — run
  // them concurrently.
  const [card, faith] = await Promise.all([
    judge(openai, skillName, composerContract, orchestratorContract, transcript),
    judgeFaithfulness(openai, composerContract, transcript),
  ]);
  printScorecard(traceId, skillName, card);
  printFaithfulness(faith);
}

async function main(): Promise<void> {
  const args = process.argv.slice(2);
  const apiKey = process.env.OPENAI_API_KEY;
  if (!apiKey) {
    console.error("OPENAI_API_KEY missing in env");
    process.exit(1);
  }
  const openai = new OpenAI({ apiKey });

  // Batch mode: judge the N most recent traces. A stepping stone toward the
  // polling worker (filtering successful runs + score ingestion come later).
  if (args[0] === "--recent") {
    const parsed = Number(args[1] ?? "5");
    const n = Number.isFinite(parsed) && parsed > 0 ? parsed : 5;
    const recent = await fetchRecentTraces(n);
    console.error(`[judge] fetched ${recent.length} recent traces`);
    for (const t of recent) {
      console.log(`\n${"—".repeat(72)}`);
      console.log(`trace ${t.id} · ${t.name} · [${t.tags.join(",")}] · ${t.timestamp}`);
      try {
        await judgeOne(openai, t.id);
      } catch (err) {
        console.error(`[judge] ${t.id} failed: ${err instanceof Error ? err.message : String(err)}`);
      }
    }
    return;
  }

  const traceId = args[0];
  if (!traceId || traceId.startsWith("--")) {
    console.error("usage: pnpm judge <traceId>  |  pnpm judge --recent [N]");
    process.exit(1);
  }
  await judgeOne(openai, traceId);
}

main().catch((err: unknown) => {
  console.error(err);
  process.exit(1);
});
