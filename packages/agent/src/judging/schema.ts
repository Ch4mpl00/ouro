import { z } from "zod";

export const JUDGE_MODEL = "gpt-5.4";
export const JUDGE_PROMPT_VERSION = "v3";

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

export const RESPONSE_SCHEMA = {
  type: "object",
  properties: {
    axes: {
      type: "array",
      items: {
        type: "object",
        properties: {
          axis: { type: "string", enum: ["coverage", "query_formulation", "composition", "process"] },
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

export const SYSTEM_PROMPT = `You are a rigorous evaluation judge for an AI agent that handles each signal in TWO stages. Understanding the split is essential to scoring correctly:

1. ORCHESTRATOR (the "planner") builds a workflow: it decides which tools to call, how to phrase and reformulate the RAG queries, which sources to hit, and how to deliver the result. Tool calls in the transcript — search_news, get_telegram_chat_history, send_telegram_message, set_memory — are ALL the orchestrator's machinery.
2. COMPOSER (the skill, e.g. news-digest / tech-digest) receives the gathered candidates and chat history as INPUT, filters them, and writes the final text (F). The composer does NOT call tools; it legitimately receives chat history as input rather than fetching it.

You score ONE completed run. You did not generate it and have no stake in it.

Inputs:
- ORCHESTRATOR_CONTRACT (planner) — how retrieval should be phrased / reformulated / routed.
- COMPOSER_CONTRACT (the skill) — how candidates should be filtered and the output composed: format, thresholds, tone, no-fabrication.
- TRANSCRIPT — the actual run: the orchestrator's RAG queries (Q) and what came back (R), plus the composer's final text (F). F is the final user-visible text of the run: the last compose step's output and the text actually delivered via send/edit tool calls in the FLOW. The trace-level FINAL OUTPUT field may be empty on the workflow path — find F in the FLOW; an empty field is NOT "no output".

Score each axis from 0 to 1 (fail < 0.3, weak < 0.5, ok < 0.75, strong >= 0.75) with a one-sentence rationale and concrete evidence (step name or item id). CRITICAL — judge each axis against the RIGHT contract:
- query_formulation -> the ORCHESTRATOR_CONTRACT (phrasing / reformulation / source routing) AND the COMPOSER_CONTRACT's stated interests/topics (what the queries should target). Did the planner's queries (Q, in the search args) cover the intent's target topics with good retrieval terms?
- coverage -> the COMPOSER_CONTRACT. Of what retrieval returned (R), did the final text (F) include the salient contract-fitting items and drop the noise?
- composition -> the COMPOSER_CONTRACT. Does F follow the composer's format, tone, length, threshold and no-fabrication rules?
- process -> the ORCHESTRATOR_CONTRACT. Walk the FLOW step by step and judge the whole chain of actions: was every tool call the right tool with sane arguments, in a sensible order; was each step's result actually used downstream (not fetched and dropped); were watermarks/memory updated when the contract requires it; did the chain deliver the result the way the contract requires? Redundant, missing, or contradictory steps lower the score.

DECISIVE RULE — never penalize the COMPOSER for ORCHESTRATION. Which tools were called, that the result was sent via send_telegram_message, that history arrived via get_telegram_chat_history, or which search tool was used are the orchestrator's job and normal workflow machinery. They are NEVER a coverage or composition violation. A composer-contract line like "do not call any Telegram tool" describes the COMPOSER's role (it composes, it doesn't fetch) — it is satisfied as long as the composer's own text doesn't try to call tools; it is NOT violated by orchestrator tool calls in the trace.

Other rules:
- Obey the contracts. If the composer contract says "< 3 matches -> short message and stop", an empty digest is CORRECT — judge whether the count was right (were there really < 3 contract-fitting, non-duplicate items in R?), not whether it produced a digest.
- If an axis does not apply to this run (nothing to deduplicate, or an empty output has no facts to verify), set applicable=false, score=null, label="n/a".
- Reward neither length nor fluency. A correct short output beats a verbose wrong one.
- Ground every claim in the TRANSCRIPT. Never invent items that aren't in R.`;

export const FAITH_SYSTEM_PROMPT = `You are a faithfulness checker for an AI-composed news digest or answer. You verify that every factual claim in the final text (F) is grounded in the retrieved snippets (R) or the chat history shown in the transcript. F is ALL user-visible text the run delivered: the text arguments of send/edit Telegram tool calls in the FLOW plus the last compose step's output. The trace-level FINAL OUTPUT field may be empty on the workflow path — verify the delivered messages from the FLOW instead; an empty field NEVER means there are no claims to check. Per the composer contract, quoting specifics — numbers, counts, dates, version numbers — not present in a snippet is the single most damaging error class.

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

export function buildUserPrompt(
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

Score this run. Return JSON matching the schema, with exactly these four axes: coverage, query_formulation, composition, process.`;
}

export function buildFaithUserPrompt(
  composerContract: string | null,
  transcript: string,
): string {
  return `<composer_contract>
${composerContract ?? "(no contract)"}
</composer_contract>

<transcript>
${transcript}
</transcript>

Extract F's atomic factual claims and verify each against R. Return JSON per the schema.`;
}

export interface JudgeResultBundle {
  scorecard: Scorecard;
  faithfulness: Faithfulness;
}
