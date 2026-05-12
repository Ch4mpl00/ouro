import "dotenv/config";
import type { ChatCompletionMessageParam } from "openai/resources/chat/completions";
import { createEngine, type Engine } from "./engine";

// Long-running process. The agent has no signal sources of its own — every
// external event (Telegram, Gmail, cron, webhook) lives inside the MCP
// server, which queues signals into its own DB and serves them with their
// matching skill instructions baked in. The agent's only job is:
//
//   loop forever:
//     {signal, pendingAfter} = mcp.get_next_signal
//     if signal is null: sleep
//     else: open session with signal.systemPrompt + handoff.md,
//           push signal.content as user message, run.
//
// All side effects (replying to Telegram, marking bills, etc.) are tool
// calls the LLM makes inside the session.
//
// Reasoning effort is **not** picked per source here. Every session starts
// at the weak default (`reasoning_effort=disabled`); the model itself
// decides whether to escalate via the in-session `handoff` tool, guided
// by `skills/handoff.md` (loaded below and appended to every system
// prompt). Keeps routing rules in markdown so dreaming can revise them
// without a code change.

const POLL_INTERVAL_MS = 2_000;

interface PendingSignal {
  id: number;
  source: string;
  content: string;
  systemPrompt: string | null;
  created_at: string;
}

interface NextSignalResult {
  signal: PendingSignal | null;
  pendingAfter: number;
}

function sleep(ms: number): Promise<void> {
  return new Promise((r) => setTimeout(r, ms));
}

// Fetched per signal via the generic `read_skill` MCP tool — keeps MCP
// unaware of handoff (it's a pure agent-side concept) and lets dreaming's
// edits to skills/handoff.md take effect on the very next signal without
// restarting the supervisor.
async function loadHandoffSkill(engine: Engine): Promise<string | null> {
  try {
    const raw = await engine.mcp.callTool("read_skill", { name: "handoff" });
    if (raw.startsWith("[tool error]")) return null;
    const parsed = JSON.parse(raw) as { content?: string };
    return parsed.content ?? null;
  } catch (err) {
    console.error("[supervisor] failed to load handoff skill:", err);
    return null;
  }
}

async function runSignal(engine: Engine, signal: PendingSignal): Promise<void> {
  if (!signal.systemPrompt) {
    console.error(`[supervisor] signal #${signal.id} source=${signal.source}: no skill, skipping`);
    return;
  }

  const handoffSkill = await loadHandoffSkill(engine);
  const systemPrompt = handoffSkill
    ? `${signal.systemPrompt}\n\n---\n\n${handoffSkill}`
    : signal.systemPrompt;

  const session = engine.startSession({
    id: `${signal.source}:${signal.id}`,
    systemPrompt,
    reasoningEffort: "disabled",
  });
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
// plain text instead of replaying it through the API.
const RECOVERY_PROMPT = [
  "The previous session handling a signal crashed. Read the message log and the error",
  "below, then send ONE short Russian message via send_telegram_message to the default",
  "chat from the environment context describing what was being done and roughly what",
  "broke. No stack traces, no error codes, no JSON. Plain language, 1–3 sentences.",
].join(" ");

async function reportFailureToUser(
  engine: Engine,
  signal: PendingSignal,
  failedMessages: ChatCompletionMessageParam[],
  err: unknown,
): Promise<void> {
  const errMsg = err instanceof Error ? err.message : String(err);
  const briefing = `Error: ${errMsg}\n\nMessage log:\n${JSON.stringify(failedMessages, null, 2)}`;
  // signal.systemPrompt is appended for the env addendum (default chat id).
  const systemPrompt = signal.systemPrompt
    ? `${RECOVERY_PROMPT}\n\n---\n\n${signal.systemPrompt}`
    : RECOVERY_PROMPT;

  const session = engine.startSession({
    id: `recovery:${signal.source}:${signal.id}`,
    systemPrompt,
    reasoningEffort: "disabled",
    maxIterations: 5,
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
