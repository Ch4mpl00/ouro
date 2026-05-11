import "dotenv/config";
import { createEngine, type Engine } from "./engine";

// Long-running process. The agent has no signal sources of its own — every
// external event (Telegram, Gmail, cron, webhook) lives inside the MCP
// server, which queues signals into its own DB and serves them with their
// matching skill instructions baked in. The agent's only job is:
//
//   loop forever:
//     {signal, pendingAfter} = mcp.get_next_signal
//     if signal is null: sleep
//     else: open session with signal.systemPrompt, push signal.content
//           as user message, run.
//
// All side effects (replying to Telegram, marking bills, etc.) are tool
// calls the LLM makes inside the session.

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

// Some signal sources need a stronger model than the default (e.g. dreaming
// edits skills based on free-form pattern recognition; thinking-tier is
// worth the cost there). Resolution: env override per-source first, then
// fall through to the engine default. Add new entries here as needed.
function resolveModelForSource(source: string): string | undefined {
  if (source === "dreaming") {
    return process.env.DREAMING_MODEL ?? "deepseek-reasoner";
  }
  return undefined;
}

async function runSignal(engine: Engine, signal: PendingSignal): Promise<void> {
  if (!signal.systemPrompt) {
    console.error(`[supervisor] signal #${signal.id} source=${signal.source}: no skill, skipping`);
    return;
  }

  const session = engine.startSession({
    id: `${signal.source}:${signal.id}`,
    systemPrompt: signal.systemPrompt,
    model: resolveModelForSource(signal.source),
  });
  session.messages.push({ role: "user", content: signal.content });

  try {
    await session.run();
  } catch (err) {
    console.error(`[supervisor] session ${session.id} crashed:`, err);
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
