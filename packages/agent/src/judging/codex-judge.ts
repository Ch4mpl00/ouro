import {
  buildFaithUserPrompt,
  FAITH_RESPONSE_SCHEMA,
  FAITH_SYSTEM_PROMPT,
  FaithfulnessSchema,
  rubricFor,
  ScorecardSchema,
  type Faithfulness,
  type JudgeNodeInput,
  type NodeJudgement,
  type Scorecard,
} from "./schema";
import type { CodexClient } from "./codex-client";

function codexConfig(): Record<string, unknown> {
  return {
    web_search: "disabled",
    "features.shell_tool": false,
    "features.multi_agent": false,
  };
}

function extractParsedJson(content: string, parsed: unknown): unknown {
  if (parsed !== undefined) return parsed;
  return JSON.parse(content);
}

function timeoutMs(): number {
  return Number(process.env.CODEX_JUDGE_TIMEOUT_MS ?? 10 * 60_000);
}

async function scorecardFor(codex: CodexClient, node: JudgeNodeInput): Promise<Scorecard> {
  const rubric = rubricFor(node.kind);
  const result = await codex.run({
    prompt: `${rubric.system}\n\nReturn only the final JSON object matching the provided schema.`,
    input: rubric.buildUserPrompt(node.skill, node.contract, node.inputText, node.outputText),
    schema: rubric.responseSchema,
    sandbox: "read-only",
    approvalPolicy: "never",
    timeoutMs: timeoutMs(),
    config: codexConfig(),
  });
  return ScorecardSchema.parse(extractParsedJson(result.content, result.parsed));
}

async function faithfulnessFor(codex: CodexClient, node: JudgeNodeInput): Promise<Faithfulness> {
  const result = await codex.run({
    prompt: `${FAITH_SYSTEM_PROMPT}\n\nReturn only the final JSON object matching the provided schema.`,
    input: buildFaithUserPrompt(node.contract, node.inputText, node.outputText),
    schema: FAITH_RESPONSE_SCHEMA,
    sandbox: "read-only",
    approvalPolicy: "never",
    timeoutMs: timeoutMs(),
    config: codexConfig(),
  });
  return FaithfulnessSchema.parse(extractParsedJson(result.content, result.parsed));
}

// Judge ONE node with codex — same contract as judgeNodeWithOpenAi.
export async function judgeNodeWithCodex(codex: CodexClient, node: JudgeNodeInput): Promise<NodeJudgement> {
  const wantsFaith = rubricFor(node.kind).faithfulness;
  const [scorecard, faithfulness] = await Promise.all([
    scorecardFor(codex, node),
    wantsFaith ? faithfulnessFor(codex, node) : Promise.resolve(null),
  ]);
  return { scorecard, faithfulness };
}
