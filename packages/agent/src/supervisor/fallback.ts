import type { ChatCompletionMessageParam } from "openai/resources/chat/completions";
import type { Engine } from "../engine";
import { buildSessionContext, type EnvData } from "../session-context";
import type { Trace } from "../tracing";

// Failure handling for the workflow path. The supervisor's main loop runs
// every signal through the workflow facade; when that fails, it hands off
// here. Two recovery shapes, keyed by the stage that failed:
//
//   compile  — the compiler couldn't produce a valid workflow. Degrade to
//              a plain agentic Session (the pre-workflow behaviour): same
//              source-matched skill + the engine's usual meta-skills.
//   execute  — the executor aborted mid-workflow. Side effects may have
//              already fired (e.g. a parallel branch sent a message before
//              another branch threw), so we do NOT retry — we surface the
//              error to the user via the recovery skill.

// Signal row as popped from the MCP queue. Owned here (rather than in
// main.ts) so fallback.ts has no import edge back to main's entrypoint,
// which runs `main()` as a top-level side effect.
export interface PendingSignal {
  id: number;
  source: string;
  content: string;
  envContext: string | null;
  created_at: string;
}

export type WorkflowFailure =
  | { stage: "compile"; reason: string; errors: string[]; attempts: number }
  | { stage: "execute"; error: unknown; reason?: string; stepIndex?: number };

export interface Fallback {
  handle(
    signal: PendingSignal,
    envData: EnvData,
    failure: WorkflowFailure,
    trace: Trace,
  ): Promise<void>;
}

export interface FallbackDeps {
  engine: Engine;
}

export function createFallback(deps: FallbackDeps): Fallback {
  const { engine } = deps;
  return {
    async handle(signal, envData, failure, trace) {
      if (failure.stage === "compile") {
        console.warn(
          `[supervisor] signal #${signal.id} compile ${failure.reason} (attempts=${failure.attempts}), falling back to agentic session`,
        );
        for (const err of failure.errors.slice(0, 3)) {
          console.warn(`[supervisor]   - ${err}`);
        }
        await runFallbackSession(engine, signal, envData, trace);
        return;
      }

      const at = failure.stepIndex !== undefined ? ` at step ${failure.stepIndex}` : "";
      const what = failure.reason ? ` ${failure.reason}` : "";
      const msg = failure.error instanceof Error ? failure.error.message : String(failure.error);
      console.error(`[supervisor] signal #${signal.id} execute${what}${at}: ${msg}`);

      await reportRunnerFailureToUser(engine, signal, failure.error, trace).catch((err) => {
        console.error(
          `[supervisor] recovery for ${signal.source}:${signal.id} also failed:`,
          err,
        );
      });
    },
  };
}

function buildPromptPrefix(sessionContext: string, envContext: string | null): string {
  return envContext ? `${sessionContext}\n\n---\n\n${envContext}` : sessionContext;
}

// Compiler couldn't produce a valid workflow — degrade to the pre-workflow
// path: spawn a Session with the source-matched skill and the engine's
// usual meta-skills. Preserves the user-facing behaviour we had before
// the workflow-mode switch.
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
        via: "fallback_after_compile",
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

// Executor aborted mid-workflow with an error. We don't know what side
// effects already fired (e.g. parallel may have sent a message before the
// other branch failed) — so we surface the error to the user via
// `recovery` and let it decide how to phrase the failure.
async function reportRunnerFailureToUser(
  engine: Engine,
  signal: PendingSignal,
  err: unknown,
  trace: Trace,
): Promise<void> {
  const errMsg = err instanceof Error ? err.message : String(err);
  const briefing = [
    `Error during workflow execution: ${errMsg}`,
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
