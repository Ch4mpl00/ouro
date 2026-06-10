import "dotenv/config";
import OpenAI from "openai";
import { createAgentDb } from "../db/client";
import { createMemoryStore } from "../db/memory";
import { createEngine, type Engine } from "../engine";
import { connectMcp } from "../mcp-client";
import { DEFAULT_PRESETS } from "../models";
import {
  createDeepseekProvider,
  createGeminiProvider,
  createOpenAiProvider,
  withRetry,
  DEEPSEEK_BASE_URL,
  GEMINI_BASE_URL,
} from "../providers";
import { gatherEnvData, type EnvDataDeps } from "../session-context";
import { createSkillStore } from "../skills";
import { nullTracer, type Tracer } from "../tracing";
import { langfuseTracerFromEnv } from "../tracing/langfuse";
import { createWorkflowRunner, type WorkflowRunner } from "../workflow";
import { createFallback, type Fallback, type PendingSignal } from "./fallback";

// Long-running supervisor — and the composition root: every long-lived
// resource (sqlite handle, providers, MCP connection, tracer, skill store)
// is built HERE and threaded down through factories. No module reaches for
// a global or reads env outside this file's wiring.
//
// The agent has no signal sources of its own — every external event
// (Telegram, Gmail, cron, webhook) lives inside the MCP server, which
// queues signals into its own DB. Each signal flows:
//
//   signal → workflow runner (compile → execute)
//          ↳ compile failed → fallback: agentic AgentLoop
//          ↳ execute failed → fallback: recovery report
//
// The poll loop and trace setup live here; everything past a workflow
// failure lives in ./fallback.

const POLL_INTERVAL_MS = 2_000;

interface NextSignalResult {
  signal: PendingSignal | null;
  pendingAfter: number;
}

function sleep(ms: number): Promise<void> {
  return new Promise((r) => setTimeout(r, ms));
}

async function runSignal(
  engine: Engine,
  envDeps: EnvDataDeps,
  signal: PendingSignal,
  runner: WorkflowRunner,
  fallback: Fallback,
): Promise<void> {
  const signalLabel = `${signal.source}:${signal.id}`;
  const envData = await gatherEnvData(envDeps);

  // One trace per signal — owns the workflow (compile + execute) and (if
  // we fall back) the session. Same sessionId so the Langfuse Sessions
  // view groups them.
  const trace = engine.tracer.trace({
    id: signalLabel,
    name: `signal:${signal.source}`,
    kind: "agent",
    sessionId: signalLabel,
    tags: [signal.source, "planner-mode"],
    metadata: {
      signal_id: signal.id,
      signal_source: signal.source,
      signal_created_at: signal.created_at,
    },
  });

  try {
    const result = await runner.runForSignal(signal, envData, trace);

    if (!result.ok) {
      await fallback.handle(signal, envData, result, trace);
      return;
    }

    console.log(
      `[supervisor] signal #${signal.id} workflow ok (attempts=${result.attempts}, steps=${result.stepCount})`,
    );
  } catch (err) {
    console.error(`[supervisor] signal #${signal.id} unexpected error:`, err);
    await fallback.handleCrash(signal, err, trace);
  } finally {
    trace.end();
  }
}

async function main(): Promise<void> {
  const deepseekApiKey = process.env.DEEPSEEK_API_KEY;
  if (!deepseekApiKey) throw new Error("DEEPSEEK_API_KEY is not set in .env");
  const openaiApiKey = process.env.OPENAI_API_KEY;
  if (!openaiApiKey) throw new Error("OPENAI_API_KEY is not set in .env");
  const geminiApiKey = process.env.GEMINI_API_KEY;
  if (!geminiApiKey) throw new Error("GEMINI_API_KEY is not set in .env (workflow compiler)");

  // Build the preset registry from defaults + per-preset env overrides.
  // `base` runs on OpenAI (non-thinking — primary replies, recovery,
  // scheduler dispatch). `smart` runs on DeepSeek with thinking on —
  // sub-agents that do real editorial / parsing work (news-digest,
  // tech-digest, nashdom-bill, …). `compiler` powers the workflow
  // compiler. `smartest` is a reserve high-end preset.
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

  // Every provider is wrapped in withRetry (429/5xx, exponential backoff):
  // the workflow compiler is on the hot path regardless of which provider
  // AGENT_COMPILER_MODEL routes to, and agentic sessions only benefit from
  // riding out a transient blip too. Each retry attempt is surfaced as a
  // WARNING `llm_retry` event on the caller's trace scope.
  const providers = {
    deepseek: withRetry(
      createDeepseekProvider(
        new OpenAI({ apiKey: deepseekApiKey, baseURL: DEEPSEEK_BASE_URL }),
      ),
    ),
    openai: withRetry(createOpenAiProvider(new OpenAI({ apiKey: openaiApiKey }))),
    gemini: withRetry(
      createGeminiProvider(
        new OpenAI({ apiKey: geminiApiKey, baseURL: GEMINI_BASE_URL }),
      ),
    ),
  };

  const db = createAgentDb();
  const memory = createMemoryStore(db);
  const skillStore = createSkillStore();
  const mcp = await connectMcp();

  // Validate every skill on disk against the live MCP registry. Crashes
  // early with a precise error if any skill is missing frontmatter, has
  // a malformed `tools:` line, or names a tool that doesn't exist —
  // instead of failing mid-signal handling.
  const mcpToolNames = mcp.tools.map((t) => t.function.name);
  await skillStore.validateAll(mcpToolNames);
  console.log(`[supervisor] skill validation passed (mcp tools: ${mcpToolNames.length})`);

  // Tracer: env auto-config > null. Logged once at startup.
  let tracer: Tracer;
  const auto = langfuseTracerFromEnv();
  if (auto) {
    tracer = auto;
    console.log(
      `[supervisor] tracing enabled (langfuse v5, ${process.env.LANGFUSE_BASE_URL ?? "default host"})`,
    );
  } else {
    tracer = nullTracer;
    console.log("[supervisor] tracing disabled (LANGFUSE_*_KEY not set)");
  }

  const engine = createEngine({
    providers,
    mcp,
    presets,
    // Meta-skills loaded into every session: routing (cross-skill
    // delegation when intent ≠ source). Only used on the fallback
    // session path now; workflow-mode signals never load these.
    skills: ["routing"],
    skillStore,
    memory,
    tracer,
  });

  console.log(`[supervisor] mcp tools: ${mcp.tools.map((t) => t.function.name).join(", ")}`);

  // Build the workflow runner once at startup: tool & skill names baked
  // into the compiler's schema enums. New tools / skills require an agent
  // restart to be emittable. The full MCP tool definitions go in so the
  // compiler can render compact `name(arg: type, ...)` signatures in its
  // user prompt — without those it guesses parameter names from
  // training-data conventions and produces invalid args.
  const skillEntries = await skillStore.listSkills();
  // Exclude `planner` itself (it's the compiler's system prompt), `routing`
  // (engine-level meta-skill, not directly invokable) and `recovery` (the
  // failure-reporting skill — only the fallback path spawns it; a workflow
  // step has no business invoking it).
  const NON_WORKFLOW_SKILLS = new Set(["planner", "routing", "recovery"]);
  const knownSkills = skillEntries
    .map((s) => s.name)
    .filter((n) => !NON_WORKFLOW_SKILLS.has(n));

  console.log(
    `[supervisor] workflow: ${mcp.tools.length} tools, ${knownSkills.length} skills`,
  );

  const runner = createWorkflowRunner({
    engine,
    // The runner (compiler + executor) needs only the skill BODY — the
    // full SkillFile shape (frontmatter tools list) stays internal to
    // engine.startAgentLoop.
    readSkill: async (name) => (await skillStore.readSkill(name))?.body ?? null,
    mcpTools: mcp.tools,
    knownSkills,
    setMemory: (key, value) => memory.set(key, value),
  });

  const fallback = createFallback({ engine });

  // Per-signal env gathering deps. USER_EMAIL is read here, once — the
  // business path (runSignal → gatherEnvData) never touches process.env.
  const envDeps: EnvDataDeps = {
    mcp,
    memory,
    userEmail: process.env.USER_EMAIL ?? null,
  };

  let stopping = false;
  const stop = async (sig: string): Promise<void> => {
    if (stopping) return;
    stopping = true;
    console.log(`[supervisor] ${sig} — shutting down`);
    await engine.shutdown();
    db.close();
    process.exit(0);
  };
  process.on("SIGINT", () => void stop("SIGINT"));
  process.on("SIGTERM", () => void stop("SIGTERM"));

  console.log("[supervisor] entering main loop (workflow-mode)");
  while (!stopping) {
    try {
      const raw = await mcp.callTool("get_next_signal", {});
      const result = JSON.parse(raw) as NextSignalResult;

      if (!result.signal) {
        await sleep(POLL_INTERVAL_MS);
        continue;
      }

      console.log(
        `[supervisor] signal #${result.signal.id} source=${result.signal.source} (${result.pendingAfter} pending after)`,
      );
      await runSignal(engine, envDeps, result.signal, runner, fallback);
    } catch (err) {
      console.error("[supervisor] loop error:", err);
      await sleep(POLL_INTERVAL_MS);
    }
  }
}

main().catch((err: unknown) => {
  console.error("[supervisor] fatal:", err);
  process.exit(1);
});
