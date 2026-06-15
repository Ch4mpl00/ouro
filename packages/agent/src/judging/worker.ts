import { assembleNodeMaterials, type NodeMaterial } from "./materials";
import { createJudgeBackend, type JudgeProvider } from "./judge-backend";
import { judgeNode } from "./node-judge";
import type { ScoreWriter } from "./langfuse-scores";
import type { TraceSource } from "./trace-source";
import { JUDGE_PROMPT_VERSION, type NodeJudgement } from "./schema";

export interface JudgeWorkerDeps {
  source: TraceSource;
  writeScores: ScoreWriter;
}

export interface JudgeWorkerOpts {
  provider: JudgeProvider;
  pollIntervalMs: number;
  recentLimit: number;
  dryRun: boolean;
  once: boolean;
}

function sleep(ms: number): Promise<void> {
  return new Promise((r) => setTimeout(r, ms));
}

function boolEnv(name: string, fallback: boolean): boolean {
  const value = process.env[name];
  if (value === undefined) return fallback;
  return ["1", "true", "yes", "on"].includes(value.toLowerCase());
}

export function judgeWorkerOptsFromEnv(): JudgeWorkerOpts {
  const provider = process.env.JUDGE_PROVIDER === "openai" ? "openai" : "codex";
  return {
    provider,
    pollIntervalMs: Number(process.env.JUDGE_POLL_INTERVAL_MS ?? 60_000),
    recentLimit: Number(process.env.JUDGE_RECENT_LIMIT ?? 20),
    dryRun: !boolEnv("JUDGE_WRITE_SCORES", false),
    once: boolEnv("JUDGE_ONCE", false),
  };
}

async function processTrace(
  traceId: string,
  deps: JudgeWorkerDeps,
  opts: JudgeWorkerOpts,
): Promise<void> {
  const { nodes } = await assembleNodeMaterials(deps.source, traceId);
  if (nodes.length === 0) {
    // No generative node to score (e.g. a pure-agentic fallback run whose only
    // generations live inside an AGENT black box). Nothing to persist; the
    // trace stays unjudged but is cheap to re-skip (no LLM call).
    console.log(`[judge-worker] ${traceId} has no judgeable nodes — skipping`);
    return;
  }

  const backend = createJudgeBackend(opts.provider);
  console.log(`[judge-worker] judging ${traceId} provider=${opts.provider} nodes=${nodes.length}`);

  // Judge ALL nodes first. If any throws (e.g. codex usage limit), we persist
  // NOTHING and the trace reappears next tick — the whole-trace auto-retry the
  // single-judgement worker had, preserved per-node. Sequential keeps codex
  // within the shared ChatGPT quota.
  const judged: Array<{ node: NodeMaterial; verdict: NodeJudgement }> = [];
  for (const node of nodes) {
    const verdict = await judgeNode(backend, node);
    judged.push({ node, verdict });
  }

  for (const { node, verdict } of judged) {
    await deps.writeScores.write(verdict.scorecard, verdict.faithfulness, {
      traceId,
      observationId: node.observationId,
      nodeKind: node.kind,
      skill: node.skill,
      provider: opts.provider,
      promptVersion: JUDGE_PROMPT_VERSION,
      dryRun: opts.dryRun,
    });
  }
}

async function tick(deps: JudgeWorkerDeps, opts: JudgeWorkerOpts): Promise<void> {
  // The local source returns only COMPLETE runs (written on trace.end()) that
  // lack ANY judgement for this (provider, version) — so no age filter and no
  // separate dedup are needed. A trace that throws (e.g. Codex usage limit)
  // writes no rows and reappears next tick: auto-retry.
  const traces = await deps.source.recentTraces(opts.recentLimit, {
    provider: opts.provider,
    promptVersion: JUDGE_PROMPT_VERSION,
  });
  for (const trace of traces) {
    try {
      await processTrace(trace.id, deps, opts);
    } catch (err) {
      const msg = err instanceof Error ? err.message : String(err);
      console.error(`[judge-worker] ${trace.id} failed: ${msg}`);
    }
  }
}

export async function runJudgeWorker(deps: JudgeWorkerDeps, opts: JudgeWorkerOpts): Promise<void> {
  console.log(
    `[judge-worker] start provider=${opts.provider} recent=${opts.recentLimit} ` +
      `interval=${opts.pollIntervalMs}ms dryRun=${opts.dryRun}`,
  );
  let stopping = false;
  process.on("SIGINT", () => {
    stopping = true;
  });
  process.on("SIGTERM", () => {
    stopping = true;
  });

  do {
    await tick(deps, opts);
    if (opts.once) break;
    await sleep(opts.pollIntervalMs);
  } while (!stopping);
}
