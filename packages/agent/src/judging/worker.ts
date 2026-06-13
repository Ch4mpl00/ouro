import OpenAI from "openai";
import { assembleMaterials } from "./materials";
import { judgeWithOpenAi } from "./openai-judge";
import { createCodexClient } from "./codex-client";
import { judgeWithCodex } from "./codex-judge";
import type { ScoreWriter } from "./langfuse-scores";
import type { TraceSource } from "./trace-source";
import { JUDGE_PROMPT_VERSION, type JudgeResultBundle } from "./schema";

type JudgeProvider = "openai" | "codex";

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

async function runJudge(
  provider: JudgeProvider,
  materials: Awaited<ReturnType<typeof assembleMaterials>>,
): Promise<JudgeResultBundle> {
  const params = {
    skillName: materials.skillName,
    composerContract: materials.composerContract,
    orchestratorContract: materials.orchestratorContract,
    transcript: materials.transcript,
  };
  if (provider === "openai") {
    const apiKey = process.env.OPENAI_API_KEY;
    if (!apiKey) throw new Error("OPENAI_API_KEY missing in env");
    return judgeWithOpenAi(new OpenAI({ apiKey }), params);
  }
  return judgeWithCodex(createCodexClient(), params);
}

async function processTrace(
  traceId: string,
  deps: JudgeWorkerDeps,
  opts: JudgeWorkerOpts,
): Promise<void> {
  const materials = await assembleMaterials(deps.source, traceId);
  console.log(
    `[judge-worker] judging ${traceId} provider=${opts.provider} skill=${materials.skillName ?? "—"} ` +
      `obs=${materials.obsCount} transcript=${materials.transcript.length}`,
  );
  const result = await runJudge(opts.provider, materials);
  await deps.writeScores.write(result.scorecard, result.faithfulness, {
    traceId,
    provider: opts.provider,
    promptVersion: JUDGE_PROMPT_VERSION,
    dryRun: opts.dryRun,
  });
}

async function tick(deps: JudgeWorkerDeps, opts: JudgeWorkerOpts): Promise<void> {
  // The local source returns only COMPLETE runs (written on trace.end()) that
  // lack a judgement for this (provider, version) — so no age filter and no
  // separate dedup are needed. A trace that throws (e.g. Codex usage limit)
  // simply writes no judgement and reappears next tick: auto-retry.
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
