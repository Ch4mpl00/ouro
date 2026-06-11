import { apiPost } from "../scripts/langfuse-api";
import { JUDGE_PROMPT_VERSION, type Faithfulness, type Scorecard } from "./schema";

export interface ScoreWriteOpts {
  traceId: string;
  provider: "openai" | "codex";
  dryRun: boolean;
}

interface LangfuseScorePayload {
  traceId: string;
  name: string;
  value: number;
  comment?: string;
  metadata?: Record<string, unknown>;
}

function comment(...parts: Array<string | null | undefined>): string {
  return parts.filter((p): p is string => Boolean(p)).join("\n");
}

function scorePayloads(
  card: Scorecard,
  faith: Faithfulness,
  opts: Omit<ScoreWriteOpts, "dryRun">,
): LangfuseScorePayload[] {
  const baseMeta = {
    judge_provider: opts.provider,
    judge_prompt_version: JUDGE_PROMPT_VERSION,
  };
  const payloads: LangfuseScorePayload[] = [];
  for (const axis of card.axes) {
    if (!axis.applicable || axis.score === null) continue;
    payloads.push({
      traceId: opts.traceId,
      name: `judge.${axis.axis}`,
      value: axis.score,
      comment: comment(axis.label, axis.rationale, axis.evidence),
      metadata: { ...baseMeta, label: axis.label },
    });
  }
  if (faith.applicable && faith.score !== null) {
    payloads.push({
      traceId: opts.traceId,
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

export async function writeLangfuseScores(
  card: Scorecard,
  faith: Faithfulness,
  opts: ScoreWriteOpts,
): Promise<void> {
  const payloads = scorePayloads(card, faith, opts);
  if (opts.dryRun) {
    for (const payload of payloads) {
      console.log(`[judge-worker] dry-run score ${payload.traceId} ${payload.name}=${payload.value}`);
    }
    return;
  }
  for (const payload of payloads) {
    await apiPost<unknown>("/scores", payload);
    console.log(`[judge-worker] wrote score ${payload.traceId} ${payload.name}=${payload.value}`);
  }
}
