import {
  buildFaithUserPrompt,
  buildUserPrompt,
  FAITH_RESPONSE_SCHEMA,
  FAITH_SYSTEM_PROMPT,
  FaithfulnessSchema,
  RESPONSE_SCHEMA,
  ScorecardSchema,
  SYSTEM_PROMPT,
  type Faithfulness,
  type JudgeResultBundle,
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

export async function judgeScorecardWithCodex(
  codex: CodexClient,
  composerSkill: string | null,
  composerContract: string | null,
  orchestratorContract: string | null,
  transcript: string,
): Promise<Scorecard> {
  const result = await codex.run({
    prompt: `${SYSTEM_PROMPT}\n\nReturn only the final JSON object matching the provided schema.`,
    input: buildUserPrompt(composerSkill, composerContract, orchestratorContract, transcript),
    schema: RESPONSE_SCHEMA,
    sandbox: "read-only",
    approvalPolicy: "never",
    timeoutMs: Number(process.env.CODEX_JUDGE_TIMEOUT_MS ?? 10 * 60_000),
    config: codexConfig(),
  });
  return ScorecardSchema.parse(extractParsedJson(result.content, result.parsed));
}

export async function judgeFaithfulnessWithCodex(
  codex: CodexClient,
  composerContract: string | null,
  transcript: string,
): Promise<Faithfulness> {
  const result = await codex.run({
    prompt: `${FAITH_SYSTEM_PROMPT}\n\nReturn only the final JSON object matching the provided schema.`,
    input: buildFaithUserPrompt(composerContract, transcript),
    schema: FAITH_RESPONSE_SCHEMA,
    sandbox: "read-only",
    approvalPolicy: "never",
    timeoutMs: Number(process.env.CODEX_JUDGE_TIMEOUT_MS ?? 10 * 60_000),
    config: codexConfig(),
  });
  return FaithfulnessSchema.parse(extractParsedJson(result.content, result.parsed));
}

export async function judgeWithCodex(
  codex: CodexClient,
  params: {
    skillName: string | null;
    composerContract: string | null;
    orchestratorContract: string | null;
    transcript: string;
  },
): Promise<JudgeResultBundle> {
  const [scorecard, faithfulness] = await Promise.all([
    judgeScorecardWithCodex(
      codex,
      params.skillName,
      params.composerContract,
      params.orchestratorContract,
      params.transcript,
    ),
    judgeFaithfulnessWithCodex(codex, params.composerContract, params.transcript),
  ]);
  return { scorecard, faithfulness };
}
