import "dotenv/config";
import type { ChatCompletionMessageParam } from "openai/resources/chat/completions";
import { createEngine, type Engine } from "./engine";
import { DEFAULT_PRESETS } from "./models";
import { buildSessionContext, gatherEnvData, type EnvData } from "./session-context";
import { listSkills, readSkill } from "./skills";
import type { Trace } from "./tracing";
import { createWorkflow, type Workflow } from "./workflow";

// Long-running process. The agent has no signal sources of its own — every
// external event (Telegram, Gmail, cron, webhook) lives inside the MCP
// server, which queues signals into its own DB.
//
//   loop forever:
//     {signal, pendingAfter} = mcp.get_next_signal
//     if signal is null: sleep
//     else: run signal through the workflow (compile → execute); fall
//           back to a plain Session if compilation can't produce a
//           valid workflow.
//
// The workflow path is the default for every signal. The session
// fallback only fires when (a) the compiler's LLM crashes / retries are
// exhausted, or (b) the executor aborts mid-workflow with an error — in
// which case we either degrade to the existing agent loop (compile
// failure) or report via `recovery` (execute failure).

const POLL_INTERVAL_MS = 2_000;

interface PendingSignal {
  id: number;
  source: string;
  content: string;
  envContext: string | null;
  created_at: string;
}

interface NextSignalResult {
  signal: PendingSignal | null;
  pendingAfter: number;
}

function sleep(ms: number): Promise<void> {
  return new Promise((r) => setTimeout(r, ms));
}

function buildPromptPrefix(sessionContext: string, envContext: string | null): string {
  return envContext ? `${sessionContext}\n\n---\n\n${envContext}` : sessionContext;
}

// Returns just the skill body (or null) — matches the surface the
// workflow facade (compiler + executor) depends on. The full SkillFile
// shape (with frontmatter tools list) stays internal to
// engine.startSession.
async function readSkillBody(name: string): Promise<string | null> {
  const s = await readSkill(name);
  return s?.body ?? null;
}

async function runSignal(
  engine: Engine,
  signal: PendingSignal,
  workflow: Workflow,
): Promise<void> {
  const signalLabel = `${signal.source}:${signal.id}`;
  const envData = await gatherEnvData(engine);

  // One trace per signal — owns the workflow (compile + execute) and (if
  // we fall back) the session. Same sessionId so the Langfuse Sessions
  // view groups them.
  const trace = engine.tracer.trace({
    id: signalLabel,
    name: `signal:${signal.source}`,
    sessionId: signalLabel,
    tags: [signal.source, "planner-mode"],
    metadata: {
      signal_id: signal.id,
      signal_source: signal.source,
      signal_created_at: signal.created_at,
    },
  });

  try {
    const result = await workflow.runForSignal(signal, envData, trace, signalLabel);

    if (!result.ok && result.stage === "compile") {
      console.warn(
        `[supervisor] signal #${signal.id} compile ${result.reason} (attempts=${result.attempts}), falling back to agentic session`,
      );
      for (const err of result.errors.slice(0, 3)) {
        console.warn(`[supervisor]   - ${err}`);
      }
      await runFallbackSession(engine, signal, envData, trace);
      return;
    }

    if (!result.ok) {
      console.error(
        `[supervisor] signal #${signal.id} execute ${result.reason} at step ${result.stepIndex}: ${result.error.message}`,
      );
      await reportRunnerFailureToUser(engine, signal, result.error, trace).catch(
        (err) => {
          console.error(`[supervisor] recovery for ${signalLabel} also failed:`, err);
        },
      );
      return;
    }

    console.log(
      `[supervisor] signal #${signal.id} workflow ok (attempts=${result.attempts}, steps=${result.stepCount})`,
    );
  } catch (err) {
    console.error(`[supervisor] signal #${signal.id} unexpected error:`, err);
    await reportRunnerFailureToUser(engine, signal, err, trace).catch((err2) => {
      console.error(`[supervisor] recovery for ${signalLabel} also failed:`, err2);
    });
  } finally {
    trace.end();
  }
}

// Planner couldn't produce a valid plan — degrade to the pre-planner
// path: spawn a Session with the source-matched skill and the engine's
// usual meta-skills. Preserves the user-facing behaviour we had before
// the planner-mode switch.
async function runFallbackSession(
  engine: Engine,
  signal: PendingSignal,
  envData: EnvData,
  trace: Trace,
): Promise<void> {
  const sessionContext = await buildSessionContext(engine, envData);

  let session;
  try {
    session = await engine.startSession({
      id: `${signal.source}:${signal.id}`,
      systemPrompt: buildPromptPrefix(sessionContext, signal.envContext),
      skills: [signal.source],
      preset: "base",
      tags: [signal.source, "agent-fallback"],
      sessionId: `${signal.source}:${signal.id}`,
      traceScope: trace,
      metadata: {
        signal_id: signal.id,
        signal_source: signal.source,
        signal_created_at: signal.created_at,
        via: "fallback_after_planner",
      },
    });
  } catch (err) {
    console.error(
      `[supervisor] signal #${signal.id} fallback session start failed: ${(err as Error).message}`,
    );
    return;
  }
  session.messages.push({ role: "user", content: signal.content });

  try {
    await session.run();
  } catch (err) {
    console.error(`[supervisor] fallback session ${session.id} crashed:`, err);
    await reportFailureToUser(engine, signal, session.messages, err, trace).catch(
      (err2) => {
        console.error(`[supervisor] recovery also failed:`, err2);
      },
    );
  } finally {
    engine.endSession(session.id);
  }
}

// Runner aborted mid-plan with an error. We don't know what side effects
// already fired (e.g. parallel may have sent a message before the other
// branch failed) — so we surface the error to the user via `recovery`
// and let it decide how to phrase the failure.
async function reportRunnerFailureToUser(
  engine: Engine,
  signal: PendingSignal,
  err: unknown,
  trace: Trace,
): Promise<void> {
  const errMsg = err instanceof Error ? err.message : String(err);
  const briefing = [
    `Error during plan execution: ${errMsg}`,
    "",
    `Signal source: ${signal.source}`,
    `Signal content (first 500 chars):`,
    signal.content.slice(0, 500),
  ].join("\n");

  await spawnRecovery(engine, signal, briefing, trace);
}

// Spawned when a primary session throws. A fresh session is used because
// the crashed message buffer may contain a malformed assistant turn that
// would re-crash on every retry — recovery reads the dead transcript as
// plain text instead of replaying it through the API. Instructions
// (what to say, how to phrase it) live in skills.default/recovery.md;
// here we only inject env-context (chat id, timezone, etc.).
async function reportFailureToUser(
  engine: Engine,
  signal: PendingSignal,
  failedMessages: ChatCompletionMessageParam[],
  err: unknown,
  trace: Trace,
): Promise<void> {
  const errMsg = err instanceof Error ? err.message : String(err);
  const briefing = `Error: ${errMsg}\n\nMessage log:\n${JSON.stringify(failedMessages, null, 2)}`;
  await spawnRecovery(engine, signal, briefing, trace);
}

async function spawnRecovery(
  engine: Engine,
  signal: PendingSignal,
  briefing: string,
  trace: Trace,
): Promise<void> {
  const session = await engine.startSession({
    id: `recovery:${signal.source}:${signal.id}`,
    skills: ["recovery"],
    includeEngineSkills: false,
    systemPrompt: signal.envContext ?? undefined,
    preset: "base",
    maxIterations: 5,
    tags: ["recovery", signal.source],
    sessionId: `${signal.source}:${signal.id}`,
    traceScope: trace,
    metadata: {
      signal_id: signal.id,
      signal_source: signal.source,
    },
  });
  session.messages.push({ role: "user", content: briefing });

  try {
    await session.run();
  } finally {
    engine.endSession(session.id);
  }
}

async function main(): Promise<void> {
  const deepseekApiKey = process.env.DEEPSEEK_API_KEY;
  if (!deepseekApiKey) throw new Error("DEEPSEEK_API_KEY is not set in .env");
  const openaiApiKey = process.env.OPENAI_API_KEY;
  if (!openaiApiKey) throw new Error("OPENAI_API_KEY is not set in .env");

  // Build the preset registry from defaults + per-preset env overrides.
  // `base` runs on OpenAI (non-thinking — primary replies, recovery,
  // scheduler dispatch). `smart` runs on DeepSeek with thinking on —
  // sub-agents that do real editorial / parsing work (news-digest,
  // tech-digest, nashdom-bill, …). `smartest` is reserved for the
  // planner role (Phase 2+); current agentic sessions never select it.
  const presets = {
    base: {
      ...DEFAULT_PRESETS.base,
      model: process.env.AGENT_BASE_MODEL ?? DEFAULT_PRESETS.base.model,
    },
    smart: {
      ...DEFAULT_PRESETS.smart,
      model: process.env.AGENT_SMART_MODEL ?? DEFAULT_PRESETS.smart.model,
    },
    smartest: {
      ...DEFAULT_PRESETS.smartest,
      model: process.env.AGENT_SMARTEST_MODEL ?? DEFAULT_PRESETS.smartest.model,
    },
  };

  const engine = await createEngine({
    deepseekApiKey,
    openaiApiKey,
    presets,
    // Meta-skills loaded into every session: routing (cross-skill
    // delegation when intent ≠ source). Only used on the fallback
    // session path now; planner-mode signals never load these.
    skills: ["routing"],
  });

  console.log(`[supervisor] mcp tools: ${engine.mcp.tools.map((t) => t.function.name).join(", ")}`);

  // Build the workflow facade once at startup: tool & skill names baked
  // into the compiler's schema enums. New tools / skills require an agent
  // restart to be emittable. The full MCP tool definitions go in so the
  // compiler can render compact `name(arg: type, ...)` signatures in its
  // user prompt — without those it guesses parameter names from
  // training-data conventions and produces invalid args.
  const skillEntries = await listSkills();
  // Exclude `planner` itself (it's the compiler's system prompt) and
  // `routing` (engine-level meta-skill, not directly invokable).
  const knownSkills = skillEntries
    .map((s) => s.name)
    .filter((n) => n !== "planner" && n !== "routing");

  console.log(
    `[supervisor] workflow: ${engine.mcp.tools.length} tools, ${knownSkills.length} skills`,
  );

  const workflow = createWorkflow({
    engine,
    readSkill: readSkillBody,
    mcpTools: engine.mcp.tools,
    knownSkills,
  });

  let stopping = false;
  const stop = async (sig: string): Promise<void> => {
    if (stopping) return;
    stopping = true;
    console.log(`[supervisor] ${sig} — shutting down`);
    await engine.shutdown();
    process.exit(0);
  };
  process.on("SIGINT", () => void stop("SIGINT"));
  process.on("SIGTERM", () => void stop("SIGTERM"));

  console.log("[supervisor] entering main loop (planner-mode)");
  while (!stopping) {
    try {
      const raw = await engine.mcp.callTool("get_next_signal", {});
      const result = JSON.parse(raw) as NextSignalResult;

      if (!result.signal) {
        await sleep(POLL_INTERVAL_MS);
        continue;
      }

      console.log(
        `[supervisor] signal #${result.signal.id} source=${result.signal.source} (${result.pendingAfter} pending after)`,
      );
      await runSignal(engine, result.signal, workflow);
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
