import OpenAI from "openai";
import type { MemoryStore } from "../db/memory";
import { fetchRecentTraces } from "../scripts/langfuse-api";
import { assembleMaterials } from "./materials";
import { judgeWithOpenAi } from "./openai-judge";
import { createCodexClient } from "./codex-client";
import { judgeWithCodex } from "./codex-judge";
import { writeLangfuseScores } from "./langfuse-scores";
import { JUDGE_PROMPT_VERSION, type JudgeResultBundle } from "./schema";

type JudgeProvider = "openai" | "codex";

export interface JudgeWorkerDeps {
  memory: MemoryStore;
}

export interface JudgeWorkerOpts {
  provider: JudgeProvider;
  pollIntervalMs: number;
  recentLimit: number;
  // Skip traces younger than this — a trace listed by /traces may still be
  // mid-run, and judging a partial transcript would mark it "ok" permanently.
  minTraceAgeMs: number;
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
    minTraceAgeMs: Number(process.env.JUDGE_MIN_TRACE_AGE_MS ?? 10 * 60_000),
    dryRun: !boolEnv("JUDGE_WRITE_SCORES", false),
    once: boolEnv("JUDGE_ONCE", false),
  };
}

function judgedKey(provider: JudgeProvider, traceId: string): string {
  return `judge.${provider}.${JUDGE_PROMPT_VERSION}.${traceId}`;
}

function isEligibleTrace(trace: { id: string; name: string; tags: string[] }): boolean {
  if (trace.tags.includes("judge")) return false;
  if (trace.name.startsWith("judge")) return false;
  return true;
}

async function runJudge(
  provider: JudgeProvider,
  materials: Awaited<ReturnType<typeof assembleMaterials>>,
): Promise<JudgeResultBundle> {
  if (provider === "openai") {
    const apiKey = process.env.OPENAI_API_KEY;
    if (!apiKey) throw new Error("OPENAI_API_KEY missing in env");
    return judgeWithOpenAi(new OpenAI({ apiKey }), {
      skillName: materials.skillName,
      composerContract: materials.composerContract,
      orchestratorContract: materials.orchestratorContract,
      transcript: materials.transcript,
    });
  }
  return judgeWithCodex(createCodexClient(), {
    skillName: materials.skillName,
    composerContract: materials.composerContract,
    orchestratorContract: materials.orchestratorContract,
    transcript: materials.transcript,
  });
}

async function processTrace(
  traceId: string,
  deps: JudgeWorkerDeps,
  opts: JudgeWorkerOpts,
): Promise<void> {
  const key = judgedKey(opts.provider, traceId);
  const current = deps.memory.get(key);
  if (current === "ok") return;
  if (current === "in_progress") {
    console.log(`[judge-worker] retrying stale in_progress trace ${traceId}`);
  }

  deps.memory.set(key, "in_progress");
  const materials = await assembleMaterials(traceId);
  console.log(
    `[judge-worker] judging ${traceId} provider=${opts.provider} skill=${materials.skillName ?? "—"} ` +
      `obs=${materials.obsCount} transcript=${materials.transcript.length}`,
  );
  const result = await runJudge(opts.provider, materials);
  await writeLangfuseScores(result.scorecard, result.faithfulness, {
    traceId,
    provider: opts.provider,
    dryRun: opts.dryRun,
  });
  deps.memory.set(key, "ok");
}

async function tick(deps: JudgeWorkerDeps, opts: JudgeWorkerOpts): Promise<void> {
  const traces = await fetchRecentTraces(opts.recentLimit);
  for (const trace of traces) {
    if (!isEligibleTrace(trace)) continue;
    if (Date.now() - new Date(trace.timestamp).getTime() < opts.minTraceAgeMs) continue;
    try {
      await processTrace(trace.id, deps, opts);
    } catch (err) {
      const msg = err instanceof Error ? err.message : String(err);
      deps.memory.set(judgedKey(opts.provider, trace.id), `error: ${msg}`);
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
