import { apiPost } from "../scripts/langfuse-api";
import type { TraceStore } from "../db/trace-store";
import { type Faithfulness, type Scorecard } from "./schema";

// Persists a judge verdict to two places: the local `judgements` table (the
// corpus the self-improvement loop queries) and Langfuse scores (the UI /
// dashboards). The local PK is the Langfuse trace id, so the two link with no
// mapping. dryRun prints and persists NOTHING — so flipping JUDGE_WRITE_SCORES
// on later re-judges every trace instead of finding them already recorded.

export interface ScoreWriteOpts {
  traceId: string;
  provider: "openai" | "codex";
  promptVersion: string;
  dryRun: boolean;
}

export interface ScoreWriter {
  write(card: Scorecard, faith: Faithfulness, opts: ScoreWriteOpts): Promise<void>;
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

// Pull the numeric value for one holistic axis (null when the judge marked it
// n/a — applicable=false or score=null).
function axisValue(card: Scorecard, axis: string): number | null {
  const a = card.axes.find((x) => x.axis === axis);
  return a && a.applicable && a.score !== null ? a.score : null;
}

function axisScores(card: Scorecard, faith: Faithfulness) {
  return {
    coverage: axisValue(card, "coverage"),
    query_formulation: axisValue(card, "query_formulation"),
    composition: axisValue(card, "composition"),
    process: axisValue(card, "process"),
    faithfulness: faith.applicable && faith.score !== null ? faith.score : null,
  };
}

function scorePayloads(
  card: Scorecard,
  faith: Faithfulness,
  opts: Pick<ScoreWriteOpts, "traceId" | "provider" | "promptVersion">,
): LangfuseScorePayload[] {
  const baseMeta = {
    judge_provider: opts.provider,
    judge_prompt_version: opts.promptVersion,
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

export function createScoreWriter(deps: {
  store: TraceStore;
  langfuseEnabled: boolean;
}): ScoreWriter {
  return {
    async write(card, faith, opts) {
      const payloads = scorePayloads(card, faith, opts);
      if (opts.dryRun) {
        for (const p of payloads) {
          console.log(`[judge] dry-run score ${p.traceId} ${p.name}=${p.value}`);
        }
        return;
      }

      // Local corpus first — it's the source of truth for the improver and
      // doesn't depend on Langfuse being up.
      deps.store.writeJudgement({
        traceId: opts.traceId,
        provider: opts.provider,
        promptVersion: opts.promptVersion,
        scores: axisScores(card, faith),
        detail: { scorecard: card, faithfulness: faith },
      });

      if (!deps.langfuseEnabled) {
        console.log(`[judge] wrote local judgement ${opts.traceId} (langfuse disabled)`);
        return;
      }
      for (const p of payloads) {
        await apiPost<unknown>("/scores", p);
        console.log(`[judge] wrote score ${p.traceId} ${p.name}=${p.value}`);
      }
    },
  };
}
