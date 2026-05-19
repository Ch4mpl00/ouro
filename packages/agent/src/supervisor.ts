import "dotenv/config";
import type { ChatCompletionMessageParam } from "openai/resources/chat/completions";
import { createEngine, type Engine } from "./engine";
import { buildSessionContext } from "./session-context";

// Long-running process. The agent has no signal sources of its own — every
// external event (Telegram, Gmail, cron, webhook) lives inside the MCP
// server, which queues signals into its own DB.
//
//   loop forever:
//     {signal, pendingAfter} = mcp.get_next_signal
//     if signal is null: sleep
//     else: open session with primary skill = signal.source,
//           push signal.content as user message, run.
//
// Skill loading is handled by the engine: meta-skill `routing` is
// configured at engine-create time and applied to every session;
// per-signal primary skill (matching `signal.source`) is passed via
// `SessionOpts.skills`. The supervisor only assembles the session-context
// + envContext block.
//
// All side effects (replying to Telegram, marking bills, etc.) are tool
// calls the LLM makes inside the session.

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

async function runSignal(engine: Engine, signal: PendingSignal): Promise<void> {
  const sessionContext = await buildSessionContext(engine);

  let session;
  try {
    session = await engine.startSession({
      id: `${signal.source}:${signal.id}`,
      systemPrompt: buildPromptPrefix(sessionContext, signal.envContext),
      skills: [signal.source],
      reasoningEffort: "disabled",
      tags: [signal.source],
      sessionId: `${signal.source}:${signal.id}`,
      metadata: {
        signal_id: signal.id,
        signal_source: signal.source,
        signal_created_at: signal.created_at,
      },
    });
  } catch (err) {
    console.error(
      `[supervisor] signal #${signal.id} source=${signal.source}: ${(err as Error).message}, skipping`,
    );
    return;
  }
  session.messages.push({ role: "user", content: signal.content });

  try {
    await session.run();
  } catch (err) {
    console.error(`[supervisor] session ${session.id} crashed:`, err);
    await reportFailureToUser(engine, signal, session.messages, err).catch((err2) => {
      console.error(`[supervisor] recovery for ${session.id} also failed:`, err2);
    });
  } finally {
    engine.endSession(session.id);
  }
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
): Promise<void> {
  const errMsg = err instanceof Error ? err.message : String(err);
  const briefing = `Error: ${errMsg}\n\nMessage log:\n${JSON.stringify(failedMessages, null, 2)}`;

  const session = await engine.startSession({
    id: `recovery:${signal.source}:${signal.id}`,
    // recovery.md carries instructions + its own minimal tools list
    // (send_telegram_message). Engine-level skills are skipped — the
    // recovery flow has no business delegating or thinking hard.
    skills: ["recovery"],
    includeEngineSkills: false,
    systemPrompt: signal.envContext ?? undefined,
    reasoningEffort: "disabled",
    maxIterations: 5,
    tags: ["recovery", signal.source],
    // Same sessionId as the primary so the crashed trace + recovery trace
    // sit together in the tracing UI's session view.
    sessionId: `${signal.source}:${signal.id}`,
    metadata: {
      signal_id: signal.id,
      signal_source: signal.source,
      crashed_with: err instanceof Error ? err.message : String(err),
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
  const apiKey = process.env.DEEPSEEK_API_KEY;
  if (!apiKey) throw new Error("DEEPSEEK_API_KEY is not set in .env");

  const engine = await createEngine({
    apiKey,
    defaultModel: process.env.AGENT_MODEL ?? "deepseek-v4-pro",
    // Meta-skills loaded into every session: routing (cross-skill
    // delegation when intent ≠ source).
    skills: ["routing"],
  });

  console.log(`[supervisor] mcp tools: ${engine.mcp.tools.map((t) => t.function.name).join(", ")}`);

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

  console.log("[supervisor] entering main loop");
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
      await runSignal(engine, result.signal);
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
