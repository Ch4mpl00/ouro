import "dotenv/config";
import "../openai-native-fetch";
import OpenAI from "openai";
import { createCodexClient } from "../codex-client";
import { createAgentDb } from "../db/client";
import { createMemoryStore } from "../db/memory";
import { createTraceStore } from "../db/trace-store";
import { createEngine } from "../engine";
import { connectMcp, RETRY_UNTIL_UP } from "../mcp-client";
import { DEFAULT_PRESETS } from "../models";
import {
  createDeepseekProvider,
  createGeminiProvider,
  createOpenAiProvider,
  withRetry,
  DEEPSEEK_BASE_URL,
  GEMINI_BASE_URL,
} from "../providers";
import type { EnvDataDeps } from "../session-context";
import { createSkillStore } from "../skills";
import type { Tracer } from "../tracing";
import { langfuseTracerFromEnv } from "../tracing/langfuse";
import { createLocalRecorderTracer } from "../tracing/local-recorder";
import { teeTracer } from "../tracing/tee";
import { createSupervisorModule, type PendingSignal } from "./module";

// Long-running supervisor — and the composition root: every long-lived
// resource (sqlite handle, providers, MCP connection, tracer, skill store)
// is built HERE and threaded down through factories. No module reaches for
// a global or reads env outside this file's wiring.
//
// The agent has no signal sources of its own — every external event
// (Telegram, Gmail, cron, webhook) lives inside the MCP server, which
// queues signals into its own DB. Each signal flows:
//
//   signal → primary AgentLoop → tools / focused sub-agents → delivery
//          ↳ crash → recovery report within the same trace
//
// The per-signal context, trace and recovery lifecycle live in ./module.

const POLL_INTERVAL_MS = 2_000;

interface NextSignalResult {
  signal: PendingSignal | null;
  pendingAfter: number;
}

function sleep(ms: number): Promise<void> {
  return new Promise((r) => setTimeout(r, ms));
}

async function main(): Promise<void> {
  const deepseekApiKey = process.env.DEEPSEEK_API_KEY;
  if (!deepseekApiKey) throw new Error("DEEPSEEK_API_KEY is not set in .env");
  const openaiApiKey = process.env.OPENAI_API_KEY;
  if (!openaiApiKey) throw new Error("OPENAI_API_KEY is not set in .env");
  const geminiApiKey = process.env.GEMINI_API_KEY;
  if (!geminiApiKey) throw new Error("GEMINI_API_KEY is not set in .env (smart agents)");

  // Primary agents and reasoning workers use smart. Recovery and simple
  // workers use base; compiler remains available to standalone workflow callers.
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

  // Provider retries appear as WARNING events in the active AgentLoop scope.
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
  const traceStore = createTraceStore(db);
  const skillStore = createSkillStore();
  // The ONE startup step allowed to be slow instead of fatal. Everything else
  // in this function (missing env var, sqlite migration failure, a skill that
  // names a tool the MCP doesn't have) is a deterministic misconfiguration:
  // it must still crash, loudly, because retrying it forever only hides it.
  // An unreachable or 500-ing MCP is different — it is the other half of a
  // two-container deploy, and the correct response is to wait for it.
  //
  // Without this, `connect()` throwing fell through to `main().catch` →
  // exit(1) → Docker restart → connect → … which is exactly how the
  // 2026-06-15 and 2026-08-23 (78 restarts) crash-loops sustained themselves.
  // See .claude/tasks/mcp-connection-lifecycle.md.
  console.log(
    `[supervisor] connecting to mcp (${process.env.MCP_TRANSPORT ?? "stdio"}${
      process.env.MCP_URL ? ` ${process.env.MCP_URL}` : ""
    })…`,
  );
  const mcp = await connectMcp({ startupRetry: RETRY_UNTIL_UP });
  console.log("[supervisor] mcp connected");
  // Sandboxed code execution (Codex service). Same client used by the
  // `code_agent` tool on both the workflow and AgentLoop paths.
  const codex = createCodexClient();

  // Validate every skill on disk against the live MCP registry. Crashes
  // early with a precise error if any skill is missing frontmatter, has
  // a malformed `tools:` line, or names a tool that doesn't exist —
  // instead of failing mid-signal handling.
  const mcpToolNames = mcp.tools.map((t) => t.function.name);
  await skillStore.validateAll(mcpToolNames);
  console.log(`[supervisor] skill validation passed (mcp tools: ${mcpToolNames.length})`);

  // Tracer: every run is mirrored into the local store (agent.db) so the
  // judge + self-improvement loop read runs fast and independently of
  // Langfuse uptime. When Langfuse creds are present we tee — Langfuse stays
  // primary (it owns the trace id; the local mirror keys on it), local is the
  // secondary leg. Without creds we record locally only.
  let tracer: Tracer;
  const local = createLocalRecorderTracer(traceStore);
  const langfuse = langfuseTracerFromEnv();
  if (langfuse) {
    tracer = teeTracer(langfuse, local);
    console.log(
      `[supervisor] tracing: langfuse v5 (${process.env.LANGFUSE_BASE_URL ?? "default host"}) + local mirror`,
    );
  } else {
    tracer = local;
    console.log("[supervisor] tracing: local mirror only (LANGFUSE_*_KEY not set)");
  }

  const engine = createEngine({
    providers,
    mcp,
    presets,
    // Root agents coordinate the task; workers opt out of these meta-skills.
    skills: ["orchestrator", "routing"],
    skillStore,
    memory,
    tracer,
    codex,
  });

  console.log(`[supervisor] mcp tools: ${mcp.tools.map((t) => t.function.name).join(", ")}`);

  // Per-signal env gathering deps. USER_EMAIL is read here, once — the
  // business path (runSignal → gatherEnvData) never touches process.env.
  const envDeps: EnvDataDeps = {
    mcp,
    memory,
    userEmail: process.env.USER_EMAIL ?? null,
  };

  const supervisor = createSupervisorModule({ engine, env: envDeps });

  let stopping = false;
  const stop = async (sig: string): Promise<void> => {
    if (stopping) return;
    stopping = true;
    console.log(`[supervisor] ${sig} — shutting down`);
    await engine.shutdown();
    db.$client.close();
    process.exit(0);
  };
  process.on("SIGINT", () => void stop("SIGINT"));
  process.on("SIGTERM", () => void stop("SIGTERM"));

  console.log("[supervisor] entering main loop (agent-loop)");
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
      await supervisor.runSignal(result.signal);
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
