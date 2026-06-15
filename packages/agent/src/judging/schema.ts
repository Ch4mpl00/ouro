import { z } from "zod";

export const JUDGE_MODEL = "gpt-5.4";
// Per-node rubrics (n1). Supersedes the whole-run v3 prompt: each generative
// node is scored against its OWNER contract, so the signal pins to one skill.
export const JUDGE_PROMPT_VERSION = "n1";

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
- query_formulation -> the PLANNER_CONTRACT's retrieval rules AND the target topics implied by the signal. Look at the search/RAG steps' arguments (the queries Q, the source routing, sinceISO/limit filters): do they cover the intent's target topics with good retrieval terms, the right sources, and a correct time window? Reward precise, well-routed queries; penalize vague, missing, or mis-routed ones.
- process -> the PLANNER_CONTRACT. Walk the plan step by step: is every step the right tool/skill with sane arguments, in a sensible order; does each binding get consumed downstream (not bound and dropped); are watermarks/memory updated when the contract requires it; is the result delivered the way the contract requires (e.g. send to the right chat); is the terminal/replan structure correct? Redundant, missing, contradictory, or dangling steps lower the score.

Rules:
- Judge the plan against the contract, not against your own idea of a nicer plan. A different-but-valid plan is not a defect.
- If the signal legitimately calls for a tiny plan (e.g. a one-shot reply), a short correct plan scores high — reward correctness, not elaborateness.
- Ground every claim in the PLAN / SIGNAL_AND_ENV. Never invent steps or queries that aren't there.`;

// ─── composer / agent node rubric ────────────────────────────────────
// Judges ONE llm_compose generation (or an llm_agent step black-box): the
// final text the node produced, against the node's skill contract and the
// input it actually received. The composer does not orchestrate — it receives
// candidates as input and writes text; never penalize it for tool calls.
export const COMPOSER_NODE_PROMPT = `You are a rigorous evaluation judge for ONE composer node of an AI agent — a single skill that receives gathered candidates (and any chat history) as INPUT and writes a final text (F). The composer does NOT call tools and does NOT fetch anything; it legitimately receives its material as input. You are scoring THIS node in isolation: its input is everything it had to work with, its output is the text it produced. You did not author it and have no stake in it.

Inputs:
- COMPOSER_CONTRACT — the skill: how candidates should be filtered and the output composed (format, thresholds, tone, length, no-fabrication). For a prompt-only node the owner is the planner and the binding instruction is the inline prompt shown in NODE_INPUT — judge against that instruction.
- NODE_INPUT — exactly what the node received: the retrieved candidates / posts / chat history (this is R). The system message is the contract above; the user message carries R.
- NODE_OUTPUT — the text the node produced (this is F).

Score EXACTLY these two axes from 0 to 1 (fail < 0.3, weak < 0.5, ok < 0.75, strong >= 0.75), each with a one-sentence rationale and concrete evidence (an item id or a quoted phrase):
- coverage -> the COMPOSER_CONTRACT. Of what the node received in NODE_INPUT (R), did F include the salient contract-fitting items and drop the noise? Missing a clearly contract-fitting item lowers it; padding with off-contract noise lowers it.
- composition -> the COMPOSER_CONTRACT. Does F follow the contract's format, tone, length, threshold, and no-fabrication rules?

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
