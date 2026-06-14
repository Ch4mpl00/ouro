import OpenAI from "openai";
import {
  buildFaithUserPrompt,
  FAITH_RESPONSE_SCHEMA,
  FAITH_SYSTEM_PROMPT,
  FaithfulnessSchema,
  JUDGE_MODEL,
  rubricFor,
  ScorecardSchema,
  type Faithfulness,
  type JudgeNodeInput,
  type NodeJudgement,
  type Scorecard,
} from "./schema";

async function scorecardFor(openai: OpenAI, node: JudgeNodeInput): Promise<Scorecard> {
  const rubric = rubricFor(node.kind);
  const res = await openai.chat.completions.create({
    model: JUDGE_MODEL,
    messages: [
      { role: "system", content: rubric.system },
      {
        role: "user",
        content: rubric.buildUserPrompt(node.skill, node.contract, node.inputText, node.outputText),
      },
    ],
    response_format: {
      type: "json_schema",
      json_schema: { name: "scorecard", strict: true, schema: rubric.responseSchema },
    },
  });
  const content = res.choices[0]?.message.content;
  if (!content) throw new Error("judge returned empty content");
  return ScorecardSchema.parse(JSON.parse(content));
}

async function faithfulnessFor(openai: OpenAI, node: JudgeNodeInput): Promise<Faithfulness> {
  const res = await openai.chat.completions.create({
    model: JUDGE_MODEL,
    messages: [
      { role: "system", content: FAITH_SYSTEM_PROMPT },
      { role: "user", content: buildFaithUserPrompt(node.contract, node.inputText, node.outputText) },
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

// Judge ONE node: its axis scorecard, plus the faithfulness pass for
// compose/agent nodes (planner nodes have no faithfulness axis). The two LLM
// calls run concurrently.
export async function judgeNodeWithOpenAi(openai: OpenAI, node: JudgeNodeInput): Promise<NodeJudgement> {
  const wantsFaith = rubricFor(node.kind).faithfulness;
  const [scorecard, faithfulness] = await Promise.all([
    scorecardFor(openai, node),
    wantsFaith ? faithfulnessFor(openai, node) : Promise.resolve(null),
  ]);
  return { scorecard, faithfulness };
}
