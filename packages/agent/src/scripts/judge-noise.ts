import "dotenv/config";
import "../openai-native-fetch";
import { readFileSync, writeFileSync } from "node:fs";
import { config as loadEnv } from "dotenv";
import OpenAI from "openai";
import { fetchRecentTraces } from "./langfuse-api";
import { createAgentDb } from "../db/client";
import { createTraceStore } from "../db/trace-store";
import { assembleNodeMaterials, type NodeMaterial } from "../judging/materials";
import { createJudgeBackend, type JudgeProvider } from "../judging/judge-backend";
import { judgeNode } from "../judging/node-judge";
import { JUDGE_MODEL, JUDGE_PROMPT_VERSION, type NodeJudgement } from "../judging/schema";
import {
  collectNodeNoise,
  summarizeNoise,
  type NodeNoise,
  type NoiseReport,
} from "../judging/noise";
import {
  createLangfuseTraceSource,
  createLocalTraceSource,
  type TraceSource,
} from "../judging/trace-source";

loadEnv({ path: ".env.agent" });

// Measure judge repeatability noise (σ_judge): re-judge the SAME nodes K times
// with an UNCHANGED judge, then report per-axis spread. The improver's gate uses
// this as the noise floor — a patch's Δ on an axis is only believable when it
// clears σ_judge. Noise is model-specific, so the result is keyed by
// (model, prompt version) and merged into a committed baseline file.
//
//   pnpm judge:noise <traceId...>            # nodes from these traces
//   pnpm judge:noise --recent 5              # nodes from 5 latest Langfuse traces
//   pnpm judge:noise --provider codex --runs 5 <traceId>
//   pnpm judge:noise --model gpt-5.4 --out path.json <traceId>

const DEFAULT_OUT = "packages/agent/src/judging/noise-baseline.json";

interface CliOpts {
  provider: JudgeProvider;
  runs: number;
  concurrency: number;
  model: string | null;
  out: string;
  recent: number | null;
  traceIds: string[];
}

function parseArgs(argv: string[]): CliOpts {
  const opts: CliOpts = {
    // codex is the PROD judge and the cheap one (shared ChatGPT quota, not the
    // expensive gpt-5.4 API) — default to it; --provider openai to override.
    provider: "codex",
    runs: 5,
    concurrency: 1,
    model: null,
    out: DEFAULT_OUT,
    recent: null,
    traceIds: [],
  };
  for (let i = 0; i < argv.length; i++) {
    const arg = argv[i]!;
    const valueOf = (flag: string): string => {
      const inline = arg.startsWith(`${flag}=`) ? arg.slice(flag.length + 1) : undefined;
      return inline ?? argv[++i] ?? "";
    };
    if (arg === "--provider" || arg.startsWith("--provider=")) {
      const v = valueOf("--provider");
      if (v !== "openai" && v !== "codex") throw new Error("--provider must be openai or codex");
      opts.provider = v;
    } else if (arg === "--runs" || arg.startsWith("--runs=")) {
      opts.runs = Math.max(2, Number(valueOf("--runs")) || 5);
    } else if (arg === "--concurrency" || arg.startsWith("--concurrency=")) {
      opts.concurrency = Math.max(1, Number(valueOf("--concurrency")) || 1);
    } else if (arg === "--model" || arg.startsWith("--model=")) {
      opts.model = valueOf("--model") || null;
    } else if (arg === "--out" || arg.startsWith("--out=")) {
      opts.out = valueOf("--out") || DEFAULT_OUT;
    } else if (arg === "--recent" || arg.startsWith("--recent=")) {
      opts.recent = Math.max(1, Number(valueOf("--recent")) || 5);
    } else if (arg.startsWith("--")) {
      throw new Error(`unknown flag: ${arg}`);
    } else {
      opts.traceIds.push(arg);
    }
  }
  return opts;
}

// Run `tasks` with at most `limit` in flight, preserving result order.
async function pool<T, R>(items: T[], limit: number, fn: (item: T, i: number) => Promise<R>): Promise<R[]> {
  const results = new Array<R>(items.length);
  let next = 0;
  async function worker(): Promise<void> {
    while (next < items.length) {
      const i = next++;
      results[i] = await fn(items[i]!, i);
    }
  }
  await Promise.all(Array.from({ length: Math.min(limit, items.length) }, worker));
  return results;
}

function modelLabel(opts: CliOpts): string {
  if (opts.model) return opts.model;
  return opts.provider === "openai" ? JUDGE_MODEL : (process.env.CODEX_JUDGE_MODEL ?? "codex");
}

async function resolveTraceIds(opts: CliOpts): Promise<string[]> {
  if (opts.recent !== null) {
    const recent = await fetchRecentTraces(opts.recent);
    return recent.map((t) => t.id);
  }
  return opts.traceIds;
}

function printReport(report: NoiseReport): void {
  console.log(
    `\n=== JUDGE NOISE · ${report.model} (${report.provider}) · prompt ${report.promptVersion} ===`,
  );
  console.log(
    `nodes=${report.sampleNodes} · runs/node=${report.runs} · judge calls=${report.totalJudgeCalls}\n`,
  );
  const head = ["axis", "nodes", "pooledσ", "meanσ", "p90σ", "maxσ", "meanScore", "flips"];
  console.log(head.map((h, i) => (i === 0 ? h.padEnd(18) : h.padStart(9))).join(" "));
  for (const a of report.axes) {
    const cells = [
      a.axis.padEnd(18),
      String(a.nodes).padStart(9),
      a.pooledSigma.toFixed(4).padStart(9),
      a.meanSigma.toFixed(4).padStart(9),
      a.p90Sigma.toFixed(4).padStart(9),
      a.maxSigma.toFixed(4).padStart(9),
      a.meanScore.toFixed(3).padStart(9),
      String(a.applicabilityFlips).padStart(9),
    ];
    console.log(cells.join(" "));
  }
  console.log(
    "\nGate guidance: use pooledσ as the per-axis noise floor; accept a patch's " +
      "Δ only when Δ > k·pooledσ (k≈2) AND no holdout regression.",
  );
}

interface BaselineEntry extends NoiseReport {
  measuredAt: string;
}

// Merge this run into the committed baseline, keyed by (model|promptVersion) so
// each judge model accumulates its own known noise; re-measuring overwrites.
function mergeBaseline(out: string, entry: BaselineEntry): void {
  let existing: Record<string, BaselineEntry> = {};
  try {
    existing = JSON.parse(readFileSync(out, "utf-8")) as Record<string, BaselineEntry>;
  } catch {
    existing = {};
  }
  existing[`${entry.model}|${entry.promptVersion}`] = entry;
  const sorted = Object.fromEntries(Object.entries(existing).sort(([a], [b]) => a.localeCompare(b)));
  writeFileSync(out, `${JSON.stringify(sorted, null, 2)}\n`);
  console.log(`\n[judge:noise] baseline updated → ${out} (key ${entry.model}|${entry.promptVersion})`);
}

async function main(): Promise<void> {
  const opts = parseArgs(process.argv.slice(2));

  let openai: OpenAI | null = null;
  if (opts.provider === "openai") {
    const apiKey = process.env.OPENAI_API_KEY;
    if (!apiKey) {
      console.error("OPENAI_API_KEY missing in env");
      process.exit(1);
    }
    openai = new OpenAI({ apiKey });
  }

  const db = createAgentDb();
  const source: TraceSource = createLocalTraceSource(
    createTraceStore(db),
    createLangfuseTraceSource(),
  );

  const traceIds = await resolveTraceIds(opts);
  if (traceIds.length === 0) {
    console.error(
      "usage: pnpm judge:noise [--provider openai|codex] [--runs K] [--concurrency C] " +
        "[--model label] [--out file] (<traceId...> | --recent N)",
    );
    process.exit(1);
  }

  // Flatten every judgeable node across the requested traces.
  const nodes: NodeMaterial[] = [];
  for (const traceId of traceIds) {
    try {
      const assembled = await assembleNodeMaterials(source, traceId);
      nodes.push(...assembled.nodes);
      console.error(`[judge:noise] ${traceId} → ${assembled.nodes.length} nodes`);
    } catch (err) {
      console.error(`[judge:noise] ${traceId} failed: ${err instanceof Error ? err.message : String(err)}`);
    }
  }
  if (nodes.length === 0) {
    console.error("[judge:noise] no judgeable nodes found");
    process.exit(1);
  }

  const backend = createJudgeBackend(opts.provider, openai);
  console.error(
    `[judge:noise] judging ${nodes.length} nodes × ${opts.runs} runs ` +
      `(provider=${opts.provider}, concurrency=${opts.concurrency})`,
  );

  // Re-judge each node K times. One flat task list keeps the concurrency cap
  // honest across nodes; codex callers leave concurrency=1 to respect the quota.
  const tasks = nodes.flatMap((node) =>
    Array.from({ length: opts.runs }, (_, run) => ({ node, run })),
  );
  let done = 0;
  const verdicts = await pool(tasks, opts.concurrency, async ({ node }) => {
    const verdict = await judgeNode(backend, node);
    done += 1;
    if (done % 5 === 0 || done === tasks.length) {
      console.error(`[judge:noise] ${done}/${tasks.length} judge calls`);
    }
    return { observationId: node.observationId, verdict };
  });

  // Regroup verdicts by node, in node order.
  const byNode = new Map<string, NodeJudgement[]>();
  for (const v of verdicts) {
    const list = byNode.get(v.observationId) ?? [];
    list.push(v.verdict);
    byNode.set(v.observationId, list);
  }
  const nodeNoise: NodeNoise[] = nodes.map((n) =>
    collectNodeNoise(
      { observationId: n.observationId, label: n.label, kind: n.kind, skill: n.skill },
      byNode.get(n.observationId) ?? [],
    ),
  );

  const report = summarizeNoise(
    {
      model: modelLabel(opts),
      provider: opts.provider,
      promptVersion: JUDGE_PROMPT_VERSION,
      runs: opts.runs,
    },
    nodeNoise,
  );
  printReport(report);
  mergeBaseline(opts.out, { ...report, measuredAt: new Date().toISOString() });
}

main().catch((err: unknown) => {
  console.error(err);
  process.exit(1);
});
