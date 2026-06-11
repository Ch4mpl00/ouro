import OpenAI from "openai";
import {
  buildFaithUserPrompt,
  buildUserPrompt,
  FAITH_RESPONSE_SCHEMA,
  FAITH_SYSTEM_PROMPT,
  FaithfulnessSchema,
  JUDGE_MODEL,
  RESPONSE_SCHEMA,
  ScorecardSchema,
  SYSTEM_PROMPT,
  type Faithfulness,
  type JudgeResultBundle,
  type Scorecard,
} from "./schema";

export async function judgeScorecardWithOpenAi(
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

export async function judgeFaithfulnessWithOpenAi(
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

export async function judgeWithOpenAi(
  openai: OpenAI,
  params: {
    skillName: string | null;
    composerContract: string | null;
    orchestratorContract: string | null;
    transcript: string;
  },
): Promise<JudgeResultBundle> {
  const [scorecard, faithfulness] = await Promise.all([
    judgeScorecardWithOpenAi(
      openai,
      params.skillName,
      params.composerContract,
      params.orchestratorContract,
      params.transcript,
    ),
    judgeFaithfulnessWithOpenAi(openai, params.composerContract, params.transcript),
  ]);
  return { scorecard, faithfulness };
}
