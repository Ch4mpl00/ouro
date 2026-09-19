// Registered before anything pulls in `openai` — the SDK auto-detects its
// shim the first time it is imported, so this stays a static import at the
// very top even though it is an entry-point concern (see
// ./openai-native-fetch). Importers of this file must therefore import it
// before ./agent-loop.
import "./openai-native-fetch";
import { Buffer } from "node:buffer";
import path from "node:path";
import { pathToFileURL } from "node:url";
import OpenAI from "openai";
import { z } from "zod";
import {
  DEEPSEEK_BASE_URL,
  DEFAULT_PRESETS,
  GEMINI_BASE_URL,
  createDeepseekProvider,
  createEngine,
  createGeminiProvider,
  createOpenAiProvider,
  createSessionContext,
  createSkillStore,
  gatherEnvData,
  isAbortError,
  isToolError,
  renderContext,
  withRetry,
  type AgentLoop,
  type Engine,
  type EnvDataDeps,
  type SessionContext,
} from "./agent-loop";
import { createCodexClient } from "./codex-client";
import {
  createAgentDb,
  createMemoryStore,
  createTraceStore,
} from "./db";
import { RETRY_UNTIL_UP, connectMcp, type McpHandle } from "./mcp-client";
import {
  createLocalRecorderTracer,
  langfuseTracerFromEnv,
  teeTracer,
  type Span,
  type Trace,
  type TraceContext,
  type Tracer,
} from "./tracing";
import { createWorkflowRunner, type WorkflowRunner } from "./workflow";

// The supervisor, end to end: the signal it pulls off the MCP queue, the
// Telegram history it preloads before the first turn, the per-signal routing
// and recovery lifecycle, and the composition root that builds the process.
//
// Read top to bottom — each section only depends on the ones above it:
//
//   signal            PendingSignal — one row of the MCP signals queue
//   telegram context  recent chat/topic history, loaded before the first LLM
//                     turn of a `telegram` signal
//   module            createSupervisorModule — routing (scheduler → workflow,
//                     everything else → AgentLoop), per-signal trace, recovery
//   main              the composition root: env, clients, tracer, engine, the
//                     poll loop. Runs only when this file IS the process
//                     entry point (`pnpm agent:start`)
//
// Everything it wires is elsewhere: ./agent-loop is the runtime, ./workflow
// the scheduler path, ./mcp-client + ./codex-client the transports, ./db the
// agent-side stores and ./tracing the observability adapters.

// ═══════════════════════════════════════════════════════════════════
// Signal
// ═══════════════════════════════════════════════════════════════════

export interface PendingSignal {
  id: number;
  source: string;
  content: string;
  envContext: string | null;
  created_at: string;
}

// ═══════════════════════════════════════════════════════════════════
// Telegram context
// ═══════════════════════════════════════════════════════════════════

export const TELEGRAM_HISTORY_KEY = "telegram.history";
export const TELEGRAM_HISTORY_LIMIT = 20;
export const TELEGRAM_HISTORY_MAX_BYTES = 8_000;

const historySchema = z.object({
  messages: z.array(z.object({
    id: z.number().int(),
    chat_id: z.number().int(),
    thread_id: z.number().int().nullable(),
    role: z.enum(["user", "assistant"]),
    text: z.string(),
    created_at: z.string(),
  }).passthrough()),
});

type HistoryMessage = z.infer<typeof historySchema>["messages"][number];
type ContextMessage = Pick<HistoryMessage, "role" | "text" | "created_at"> & { truncated?: boolean };

function timestamp(value: string): number {
  // MCP's SQLite timestamps are UTC without an offset.
  return Date.parse(/^\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2}$/.test(value)
    ? value.replace(" ", "T") + "Z"
    : value);
}

function inlineHistory(history: HistoryMessage[]): { messages: ContextMessage[]; truncated: boolean } {
  const messages: ContextMessage[] = [];
  for (const { role, text, created_at } of [...history].reverse()) {
    const message: ContextMessage = { role, text, created_at };
    if (Buffer.byteLength(JSON.stringify([message, ...messages])) > TELEGRAM_HISTORY_MAX_BYTES) {
      // Prefer whole recent messages. If even the newest one is too large,
      // show its tail (where follow-up offers usually live), explicitly marked.
      if (messages.length === 0) {
        const clipped: ContextMessage = { ...message, text: "…", truncated: true };
        let remaining = TELEGRAM_HISTORY_MAX_BYTES - Buffer.byteLength(JSON.stringify([clipped]));
        const tail: string[] = [];
        for (const character of Array.from(text).reverse()) {
          const bytes = Buffer.byteLength(JSON.stringify(character)) - 2;
          if (bytes > remaining) break;
          remaining -= bytes;
          tail.push(character);
        }
        clipped.text += tail.reverse().join("");
        messages.push(clipped);
      }
      return { messages, truncated: true };
    }
    messages.unshift(message);
  }
  return { messages, truncated: false };
}

// Source-specific preparation belongs to the supervisor, not to every loop:
// workers and scheduler/domain signals must not auto-fetch Telegram history.
export async function prepareTelegramInput(
  signal: PendingSignal,
  context: SessionContext,
  mcp: Pick<McpHandle, "callTool">,
  trace: TraceContext,
  abortSignal?: AbortSignal,
): Promise<string> {
  abortSignal?.throwIfAborted();
  if (signal.source !== "telegram") return signal.content;

  // This header is emitted by the Telegram poller. Anchor it so quoted text
  // or a default delivery target in envContext cannot select another chat.
  const header = /^Telegram message in chat (-?\d+)(?: \(forum topic thread_id=(\d+)\))?\.\nText: ([\s\S]+)$/.exec(signal.content);
  const unavailable = () => `Recent Telegram history is unavailable. Use the current request; fetch context only if needed.\n\n## Current Telegram signal\n${signal.content}`;
  if (!header) return unavailable();

  const chatId = header[1]!;
  const threadId = header[2] === undefined ? null : Number(header[2]);
  const args = { chatId, limit: TELEGRAM_HISTORY_LIMIT, ...(threadId === null ? {} : { threadId }) };
  const span = trace.span({ name: "get_telegram_chat_history", kind: "tool", input: args, metadata: { automatic: true } });
  try {
    const currentText = z.string().parse(JSON.parse(header[3]!));
    const signalTime = timestamp(signal.created_at);
    if (!Number.isFinite(signalTime)) throw new Error("Invalid Telegram signal timestamp");
    const raw = await mcp.callTool("get_telegram_chat_history", args, { signal: abortSignal });
    abortSignal?.throwIfAborted();
    if (isToolError(raw)) throw new Error(raw);
    const result = historySchema.parse(JSON.parse(raw));
    // Omitted threadId means all topics in the existing MCP API. Keep only
    // the originating topic (including General), and no later queued messages.
    const scoped = result.messages.filter((m) =>
      String(m.chat_id) === chatId && m.thread_id === threadId && timestamp(m.created_at) <= signalTime,
    );
    // The poller records the message immediately before queuing the signal.
    // Match that occurrence, not an identical phrase from an older exchange.
    const current = scoped.findIndex((m) =>
      m.role === "user" && m.text === currentText && timestamp(m.created_at) >= signalTime - 1_000,
    );
    const history = scoped.slice(0, current === -1 ? scoped.length : current);
    context.memory.put(TELEGRAM_HISTORY_KEY, JSON.stringify({ messages: history }), "json");
    const inline = inlineHistory(history);
    span.update({ metadata: { memory_key: TELEGRAM_HISTORY_KEY, history_messages: history.length, inline_truncated: inline.truncated } });
    span.end({ output: raw });
    return [
      "## Recent Telegram history",
      "Automatically loaded context, not a new request. Messages are in chronological order.",
      `Full fetched history: memory_key=${TELEGRAM_HISTORY_KEY}. Pass this key in input_refs when delegating.`,
      `Shown ${inline.messages.length} of ${history.length} prior messages; truncated=${inline.truncated}.`,
      JSON.stringify(inline.messages),
      "",
      "## Current Telegram signal",
      signal.content,
    ].join("\n");
  } catch (err) {
    const error = err instanceof Error ? err.message : String(err);
    span.end({ output: { error }, level: "ERROR", statusMessage: error });
    abortSignal?.throwIfAborted();
    return unavailable();
  }
}

// ═══════════════════════════════════════════════════════════════════
// Module
// ═══════════════════════════════════════════════════════════════════

export interface SupervisorModule {
  runSignal(signal: PendingSignal): Promise<string>;
  shutdown(): Promise<void>;
}

export interface SupervisorModuleDeps {
  engine: Engine;
  env: EnvDataDeps;
  workflow: WorkflowRunner;
}

// Routing is deterministic: scheduler uses compile → execute; every other
// source uses AgentLoop. Both paths share one context and trace per signal.
export function createSupervisorModule({ engine, env, workflow }: SupervisorModuleDeps): SupervisorModule {
  const cancellation = new AbortController();
  const active = new Set<Promise<string>>();
  let shutdown: Promise<void> | undefined;
  async function recover(signal: PendingSignal, context: SessionContext, error: string, loop: AgentLoop | undefined, trace: Trace): Promise<void> {
    const span = trace.span({ name: "recovery", kind: "agent", metadata: { skill: "recovery" } });
    const id = `${context.id}__recovery`;
    try {
      const recovery = await engine.startAgentLoop({
        id,
        signal: cancellation.signal,
        sessionContext: context,
        parentId: context.id,
        skills: ["recovery"],
        includeEngineSkills: false,
        systemPrompt: [renderContext(context.env), signal.envContext].filter(Boolean).join("\n\n"),
        preset: "base",
        maxIterations: 5,
        traceScope: span,
      });
      const transcript = JSON.stringify(loop?.messages ?? []);
      const output = await recovery.send([
        `Original signal (${signal.source}):\n${signal.content}`,
        `Error: ${error}`,
        `Recent transcript (may be truncated):\n${transcript.slice(-20_000)}`,
        "Report the failure only. Do not repeat the original actions; some may already have succeeded.",
      ].join("\n\n"));
      span.end({ output });
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      span.end({ output: { error: message }, level: "ERROR", statusMessage: message });
      engine.log(context.id, `recovery failed: ${message}`);
    } finally {
      await engine.endAgentLoop(id);
    }
  }

  async function runSignal(signal: PendingSignal): Promise<string> {
    const id = `${signal.source}:${signal.id}`;
    const useWorkflow = signal.source === "scheduler";
    const trace = engine.tracer.trace({
      id,
      name: `signal:${signal.source}`,
      kind: "agent",
      sessionId: id,
      tags: [signal.source, useWorkflow ? "planner-mode" : "agent-loop"],
      metadata: { signal_id: signal.id, signal_source: signal.source, signal_created_at: signal.created_at },
    });
    trace.update({ input: signal.content });
    let context: SessionContext | undefined;
    let loop: AgentLoop | undefined;
    let span: Span | undefined;
    try {
      context = createSessionContext({ id, env: await gatherEnvData(env, cancellation.signal) });
      if (useWorkflow) {
        const result = await workflow.runForSignal(signal, context, trace, cancellation.signal);
        cancellation.signal.throwIfAborted();
        if (result.ok) {
          const summary = { ok: true, attempts: result.attempts, stepCount: result.stepCount };
          trace.update({ output: summary });
          engine.log(id, `workflow ok (attempts=${result.attempts}, steps=${result.stepCount})`);
          return JSON.stringify(summary);
        }
        if (result.stage === "execute") {
          // An earlier step may already have delivered. Report the error;
          // never restart the task in a fresh agentic loop after execution.
          trace.event({ name: "fallback", level: "ERROR", metadata: {
            stage: result.stage, reason: result.reason, step_index: result.stepIndex,
          } });
          throw new Error(`Workflow execution failed at step ${result.stepIndex} (${result.reason}): ${result.error.message}`);
        }
        // Preserve the previous compile/replan fallback. The same context
        // and trace continue into the agentic path below.
        trace.event({ name: "fallback", level: "WARNING", metadata: {
          stage: result.stage,
          reason: result.stage === "compile" ? result.reason : "replan_exhausted",
          attempts: result.attempts,
          ...(result.stage === "compile" ? { errors: result.errors } : { passes: result.passes }),
        } });
        trace.update({ metadata: { fallback: "agent-loop", workflow_failure_stage: result.stage } });
        engine.log(id, `workflow ${result.stage} failed; falling back to AgentLoop`);
      }
      // A chain groups the primary loop without hiding its child AGENT nodes
      // from the per-node judge, which treats AGENT descendants as a black box.
      span = trace.span({ name: "agent_loop", kind: "chain" });
      const input = await prepareTelegramInput(signal, context, engine.mcp, trace, cancellation.signal);
      // Transport skills describe delivery. Domain skills belong in workers;
      // a new source is a delegation hint, so adding one needs no code change.
      const transport = signal.source === "telegram" || signal.source === "scheduler";
      loop = await engine.startAgentLoop({
        id,
        signal: cancellation.signal,
        sessionContext: context,
        skills: transport ? [signal.source] : [],
        systemPrompt: [
          renderContext(context.env),
          `Signal source: ${signal.source}`,
          transport ? "" : `Delegate the domain work to skill ${JSON.stringify(signal.source)}. You own the final delivery.`,
          signal.envContext,
        ].filter(Boolean).join("\n\n"),
        preset: useWorkflow ? "base" : "smart",
        traceScope: span,
      });
      const output = await loop.send(input);
      span.end({ output });
      trace.update({ output, metadata: { skills: transport ? [signal.source, "orchestrator", "routing"] : ["orchestrator", "routing"] } });
      return output;
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      span?.end({ output: { error: message }, level: "ERROR", statusMessage: message });
      trace.update({ output: { error: message }, metadata: { error: true } });
      if (context && !cancellation.signal.aborted && !isAbortError(err)) await recover(signal, context, message, loop, trace);
      cancellation.signal.throwIfAborted();
      throw err;
    } finally {
      if (span) await engine.endAgentLoop(id);
      trace.end();
    }
  }

  return {
    runSignal(signal) {
      if (cancellation.signal.aborted) return Promise.reject(cancellation.signal.reason);
      const run = runSignal(signal).finally(() => { active.delete(run); });
      active.add(run);
      return run;
    },
    shutdown() {
      cancellation.abort();
      shutdown ??= (async () => {
        // Supervisor owns the signal trace. Finish recovery/trace cleanup before
        // the engine flushes tracing or closes resources shared by all signals.
        await Promise.allSettled([...active]);
        await engine.shutdown();
      })();
      return shutdown;
    },
  };
}

// ═══════════════════════════════════════════════════════════════════
// Main
// ═══════════════════════════════════════════════════════════════════

// Long-running supervisor — and the composition root: every long-lived
// resource (sqlite handle, providers, MCP connection, tracer, skill store)
// is built HERE and threaded down through factories. No module reaches for
// a global or reads env outside this file's wiring.
//
// The agent has no signal sources of its own — every external event
// (Telegram, Gmail, cron, webhook) lives inside the MCP server, which
// queues signals into its own DB. Each signal flows:
//
//   scheduler → workflow (compile → execute)
//              ↳ compile failure → AgentLoop; execution failure → recovery
//   other sources → primary AgentLoop → tools / focused sub-agents → delivery
//                   ↳ crash → recovery report within the same trace
//
// The per-signal context, trace and recovery lifecycle live in the Module
// section above.

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
  // workers use base; compiler plans scheduler workflows.
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

  // Compile against the available tools and domain skills. Meta-skills belong
  // to the supervisor or the compiler, not to workflow work steps.
  const nonWorkflowSkills = new Set(["planner", "orchestrator", "routing", "recovery"]);
  const knownSkills = (await skillStore.listSkills())
    .map((skill) => skill.name)
    .filter((name) => !nonWorkflowSkills.has(name));
  const workflow = createWorkflowRunner({
    engine,
    mcpTools: mcp.tools,
    knownSkills,
    readSkill: async (name) => (await skillStore.readSkill(name))?.body ?? null,
    readPatch: (name) => skillStore.readPatch(name),
    setMemory: (key, value) => memory.set(key, value),
    codex,
  });
  const supervisor = createSupervisorModule({ engine, env: envDeps, workflow });

  let stopping = false;
  const stop = async (sig: string): Promise<void> => {
    if (stopping) return;
    stopping = true;
    console.log(`[supervisor] ${sig} — shutting down`);
    await supervisor.shutdown();
    db.$client.close();
    process.exit(0);
  };
  process.on("SIGINT", () => void stop("SIGINT"));
  process.on("SIGTERM", () => void stop("SIGTERM"));

  console.log("[supervisor] entering main loop (scheduler → workflow, other sources → agent-loop)");
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
      if (stopping) break;
      console.error("[supervisor] loop error:", err);
      await sleep(POLL_INTERVAL_MS);
    }
  }
}

// This file is both the process entry point (`pnpm agent:start`) and the
// module the tests import, so the composition root only runs when it IS the
// entry. `dotenv` is loaded here rather than at the top: reading a
// developer's .env is an entry-point concern, not something an importer of
// this file should get as a side effect.
if (process.argv[1] && import.meta.url === pathToFileURL(path.resolve(process.argv[1])).href) {
  await import("dotenv/config");
  main().catch((err: unknown) => {
    console.error("[supervisor] fatal:", err);
    process.exit(1);
  });
}
