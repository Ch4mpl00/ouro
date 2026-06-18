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
import type { JudgeBackend } from "./judge-backend";

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
