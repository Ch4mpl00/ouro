import "dotenv/config";
import "../../../openai-native-fetch";
import { config as loadEnv } from "dotenv";
import OpenAI from "openai";
import { createCodexClient } from "../../../codex-client";
import { createAgentDb } from "../../../db/client";
import { createMemoryStore } from "../../../db/memory";
import { createTraceStore } from "../../../db/trace-store";
import { createEngine } from "../../../engine";
import { connectMcp } from "../../../mcp-client";
import { DEFAULT_PRESETS } from "../../../models";
import {
  createDeepseekProvider,
  createGeminiProvider,
  createOpenAiProvider,
  withRetry,
  DEEPSEEK_BASE_URL,
  GEMINI_BASE_URL,
} from "../../../providers";
import { gatherEnvData, type EnvDataDeps } from "../../../session-context";
import { createSkillStore } from "../../../skills";
import { createLocalRecorderTracer } from "../../../tracing/local-recorder";
import { createWorkflowRunner, type WorkflowSignal } from "../../../workflow";
import { createBenchMcpClient } from "./bench-mcp-client";
import { downloadAttachment, loadGaiaTasks, type GaiaLevel, type GaiaTask } from "./dataset";
import { questionScorer } from "./scorer";

loadEnv({ path: ".env.agent" });

// GAIA Tier-1 harness (workflow path). Drives the prod plan→act→replan loop
// (`createWorkflowRunner`) over GAIA questions behind a `BenchMCPClient` — the
// same DI seam the e2e sandbox uses: no prod tool code touched, no prod writes
// (side-effect tools are suppressed). Scores each final answer with the
// official GAIA scorer and prints a per-level table.
//
// Run:  MCP_NO_POLLERS=1 pnpm bench:gaia --level 1 --max-tasks 10
//   (or point at a remote MCP: MCP_TRANSPORT=http MCP_URL=... pnpm bench:gaia ...)

interface CliOpts {
  level: GaiaLevel | "all";
  maxTasks: number;
}

function parseArgs(argv: string[]): CliOpts {
  let level: GaiaLevel | "all" = "all";
  let maxTasks = 10;
  for (let i = 0; i < argv.length; i++) {
    const arg = argv[i];
    if (arg === "--level") {
      const v = argv[++i];
      if (v === "all") level = "all";
      else if (v === "1" || v === "2" || v === "3") level = Number(v) as GaiaLevel;
      else throw new Error("--level must be 1, 2, 3, or all");
    } else if (arg === "--max-tasks") {
      maxTasks = Number(argv[++i]);
      if (!Number.isFinite(maxTasks) || maxTasks <= 0) throw new Error("--max-tasks must be a positive number");
    }
  }
  return { level, maxTasks };
}

// Outcome category — a coarse first cut at the failure taxonomy. The full
// tool-coverage-vs-loop/reasoning split is PR2; here we just separate the
// buckets the harness can already tell apart.
type Outcome =
  | "correct"
  | "wrong"
  | "no_answer" // workflow ok but never bound `answer` (contract miss)
  | "compile_fail"
  | "execute_fail"
  | "replan_exhausted"
  | "crash";

interface TaskResult {
  task: GaiaTask;
  outcome: Outcome;
  predicted: string | null;
}

async function main(): Promise<void> {
  const opts = parseArgs(process.argv.slice(2));

  const deepseekApiKey = process.env.DEEPSEEK_API_KEY;
  if (!deepseekApiKey) throw new Error("DEEPSEEK_API_KEY is not set in .env.agent");
  const openaiApiKey = process.env.OPENAI_API_KEY;
  if (!openaiApiKey) throw new Error("OPENAI_API_KEY is not set in .env.agent");

  // Default presets route base/smartest/compiler → OpenAI, smart → DeepSeek;
  // no preset routes to Gemini, so its key is optional (the provider is built
  // anyway to satisfy the engine's shape, but never invoked unless an env
  // override points a model name at "gemini-*").
  const geminiApiKey = process.env.GEMINI_API_KEY ?? "";

  const withEnvModel = (name: keyof typeof DEFAULT_PRESETS, envVar: string) => ({
    ...DEFAULT_PRESETS[name],
    model: process.env[envVar] ?? DEFAULT_PRESETS[name].model,
  });
  const presets = {
    base: withEnvModel("base", "AGENT_BASE_MODEL"),
    smart: withEnvModel("smart", "AGENT_SMART_MODEL"),
    smartest: withEnvModel("smartest", "AGENT_SMARTEST_MODEL"),
    compiler: withEnvModel("compiler", "AGENT_COMPILER_MODEL"),
  };

  const providers = {
    deepseek: withRetry(
      createDeepseekProvider(new OpenAI({ apiKey: deepseekApiKey, baseURL: DEEPSEEK_BASE_URL })),
    ),
    openai: withRetry(createOpenAiProvider(new OpenAI({ apiKey: openaiApiKey }))),
    gemini: withRetry(
      createGeminiProvider(new OpenAI({ apiKey: geminiApiKey, baseURL: GEMINI_BASE_URL })),
    ),
  };

  const db = createAgentDb();
  const memory = createMemoryStore(db);
  const traceStore = createTraceStore(db);
  const skillStore = createSkillStore();
  const codex = createCodexClient();

  // Zero-infra default: with no MCP endpoint configured but a Tavily key
  // present, point straight at Tavily's hosted MCP (web search/extract only).
  // Enough for a web-lookup L1 slice without standing up our own MCP + PG.
  // Set MCP_TRANSPORT/MCP_URL explicitly to use the full own-MCP toolbelt.
  if (!process.env.MCP_URL && process.env.TAVILY_API_KEY) {
    process.env.MCP_TRANSPORT = "http";
    process.env.MCP_URL = `https://mcp.tavily.com/mcp/?tavilyApiKey=${process.env.TAVILY_API_KEY}`;
    console.log("[bench] no MCP_URL set — using Tavily-hosted MCP directly (search/extract only)");
  }

  const realMcp = await connectMcp();
  const mcp = createBenchMcpClient(realMcp);
  console.log(`[bench] toolbelt: ${mcp.tools.map((t) => t.function.name).join(", ")}`);

  const tracer = createLocalRecorderTracer(traceStore);
  const engine = createEngine({
    providers,
    mcp,
    presets,
    skills: [],
    skillStore,
    memory,
    tracer,
    codex,
  });

  const skillEntries = await skillStore.listSkills();
  const NON_WORKFLOW_SKILLS = new Set(["planner", "routing", "recovery"]);
  const knownSkills = skillEntries.map((s) => s.name).filter((n) => !NON_WORKFLOW_SKILLS.has(n));

  const runner = createWorkflowRunner({
    engine,
    readSkill: async (name) => (await skillStore.readSkill(name))?.body ?? null,
    readPatch: (name) => skillStore.readPatch(name),
    mcpTools: mcp.tools,
    knownSkills,
    setMemory: (key, value) => memory.set(key, value),
    codex,
    // Lift the autonomous-loop ceiling for L2/L3 long-horizon tasks; PR2 will
    // tag where even this is too shallow.
    maxPasses: opts.level === 1 ? 3 : 5,
  });

  const envDeps: EnvDataDeps = {
    mcp,
    memory,
    userEmail: process.env.USER_EMAIL ?? null,
  };

  const tasks = await loadGaiaTasks({ level: opts.level, maxTasks: opts.maxTasks });
  console.log(`[bench] running ${tasks.length} GAIA task(s) (level=${opts.level})\n`);

  const results: TaskResult[] = [];
  for (const [i, task] of tasks.entries()) {
    const result = await runOne(i, task, runner, engine, envDeps);
    results.push(result);
    const gold = task.finalAnswer || "(no gold)";
    console.log(
      `  [${i + 1}/${tasks.length}] L${task.level} ${task.taskId.slice(0, 8)} → ${result.outcome}` +
        `  pred=${JSON.stringify(result.predicted)} gold=${JSON.stringify(gold)}`,
    );
  }

  printReport(results, mcp.sideEffectLog.length);

  await mcp.close();
  await engine.shutdown();
}

async function runOne(
  index: number,
  task: GaiaTask,
  runner: ReturnType<typeof createWorkflowRunner>,
  engine: ReturnType<typeof createEngine>,
  envDeps: EnvDataDeps,
): Promise<TaskResult> {
  // GAIA attachments are addressed by local path; pass it in the per-signal
  // env addendum (the channel prod uses for source-specific context).
  const filePath = await downloadAttachment(task);
  const envContext = [
    "## GAIA bench task",
    "Answer the question in `signal.content`. Finish with an `llm_compose`",
    "step (skill `gaia`) that binds the final answer to the variable `answer`,",
    "formatted per the gaia skill's strict rules.",
    filePath ? `Attached file (read with read_pdf / read_file): ${filePath}` : "No attached file.",
  ].join("\n");

  const signal: WorkflowSignal = {
    id: 900_000 + index,
    source: "gaia",
    content: task.question,
    envContext,
  };

  const signalLabel = `gaia:${signal.id}`;
  const trace = engine.tracer.trace({
    id: signalLabel,
    name: "signal:gaia",
    kind: "agent",
    sessionId: signalLabel,
    tags: ["gaia", "bench", `level-${task.level}`],
    metadata: { gaia_task_id: task.taskId, gaia_level: task.level },
  });

  try {
    const envData = await gatherEnvData(envDeps);
    const result = await runner.runForSignal(signal, envData, trace);

    if (!result.ok) {
      const outcome: Outcome =
        result.stage === "compile"
          ? "compile_fail"
          : result.stage === "execute"
            ? "execute_fail"
            : "replan_exhausted";
      return { task, outcome, predicted: null };
    }

    const predicted = result.store.has("answer") ? String(result.store.get("answer")) : null;
    if (predicted === null) return { task, outcome: "no_answer", predicted: null };

    const correct = task.finalAnswer ? questionScorer(predicted, task.finalAnswer) : false;
    return { task, outcome: correct ? "correct" : "wrong", predicted };
  } catch (err) {
    console.error(`  [task ${task.taskId}] crashed:`, err);
    return { task, outcome: "crash", predicted: null };
  } finally {
    trace.end();
  }
}

function printReport(results: TaskResult[], suppressedCalls: number): void {
  console.log("\n=== GAIA results ===");
  const levels: GaiaLevel[] = [1, 2, 3];
  for (const level of levels) {
    const rows = results.filter((r) => r.task.level === level);
    if (rows.length === 0) continue;
    const correct = rows.filter((r) => r.outcome === "correct").length;
    const pct = ((correct / rows.length) * 100).toFixed(1);
    console.log(`  L${level}: ${correct}/${rows.length} correct (${pct}%)`);
  }
  const total = results.length;
  const totalCorrect = results.filter((r) => r.outcome === "correct").length;
  console.log(`  ALL: ${totalCorrect}/${total} correct (${((totalCorrect / total) * 100).toFixed(1)}%)`);

  console.log("\n  outcome breakdown:");
  const buckets = new Map<Outcome, number>();
  for (const r of results) buckets.set(r.outcome, (buckets.get(r.outcome) ?? 0) + 1);
  for (const [outcome, n] of [...buckets.entries()].sort((a, b) => b[1] - a[1])) {
    console.log(`    ${outcome}: ${n}`);
  }
  if (suppressedCalls > 0) {
    console.log(`\n  (${suppressedCalls} side-effect tool call(s) suppressed by the bench client)`);
  }
}

main().catch((err) => {
  console.error("bench:gaia crashed:", err);
  process.exit(1);
});
