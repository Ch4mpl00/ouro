import type { ChatCompletionMessageParam } from "openai/resources/chat/completions";
import type { Engine } from "../engine";
import { buildSessionContext, type EnvData } from "../session-context";
import type { Trace } from "../tracing";
import type { WorkflowRunFailure } from "../workflow";

// Failure handling for the workflow path. The supervisor's main loop runs
// every signal through the workflow runner; when that fails, it hands the
// failure here AS-IS (the runner's discriminated union — no re-encoding in
// main.ts). Routing by stage:
//
//   compile / replan_exhausted — the compiler couldn't produce (or commit
//              to) a valid workflow. Degrade to a plain agentic AgentLoop
//              (the pre-workflow behaviour): same source-matched skill +
//              the engine's usual meta-skills.
//   execute  — the executor aborted mid-workflow. Side effects may have
//              already fired (e.g. a parallel branch sent a message before
//              another branch threw), so we do NOT retry — we surface the
//              error to the user via the recovery skill.
//
// This module also owns the `fallback` trace event (the timeline marker
// for the workflow→agentic handoff), so the supervisor's loop stays thin.

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

export interface Fallback {
  // Workflow runner returned a failure result.
  handle(
    signal: PendingSignal,
    envData: EnvData,
    failure: WorkflowRunFailure,
    trace: Trace,
  ): Promise<void>;
  // Workflow runner THREW (unexpected crash outside the result union).
  handleCrash(signal: PendingSignal, err: unknown, trace: Trace): Promise<void>;
}

export interface FallbackDeps {
  engine: Engine;
}

export function createFallback(deps: FallbackDeps): Fallback {
  const { engine } = deps;

  async function degradeToAgentic(
    signal: PendingSignal,
    envData: EnvData,
    reason: string,
    errors: string[],
    attempts: number,
    trace: Trace,
  ): Promise<void> {
    // A compile miss is an expected degradation — WARNING, not ERROR.
    trace.event({
      name: "fallback",
      level: "WARNING",
      metadata: { stage: "compile", reason, attempts },
    });
    console.warn(
      `[supervisor] signal #${signal.id} compile ${reason} (attempts=${attempts}), falling back to agentic session`,
    );
    for (const err of errors.slice(0, 3)) {
      console.warn(`[supervisor]   - ${err}`);
    }
    await runFallbackAgentLoop(engine, signal, envData, trace);
  }

  async function reportExecuteFailure(
    signal: PendingSignal,
    err: unknown,
    reason: string | undefined,
    stepIndex: number | undefined,
    trace: Trace,
  ): Promise<void> {
    // Execute failure means side effects may already have fired — ERROR.
    trace.event({
      name: "fallback",
      level: "ERROR",
      metadata: {
        stage: "execute",
        ...(reason !== undefined ? { reason } : {}),
        ...(stepIndex !== undefined ? { step_index: stepIndex } : {}),
        error: err instanceof Error ? err.message : String(err),
      },
    });
    const at = stepIndex !== undefined ? ` at step ${stepIndex}` : "";
    const what = reason ? ` ${reason}` : "";
    const msg = err instanceof Error ? err.message : String(err);
    console.error(`[supervisor] signal #${signal.id} execute${what}${at}: ${msg}`);

    await reportRunnerFailureToUser(engine, signal, err, trace).catch((err2) => {
      console.error(
        `[supervisor] recovery for ${signal.source}:${signal.id} also failed:`,
        err2,
      );
    });
  }

  return {
    async handle(signal, envData, failure, trace) {
      switch (failure.stage) {
        case "compile":
          await degradeToAgentic(
            signal,
            envData,
            failure.reason,
            failure.errors,
            failure.attempts,
            trace,
          );
          return;
        case "replan_exhausted":
          // The planner replanned on every pass without committing. Treat
          // it like a compile miss — degrade to an agentic session.
          await degradeToAgentic(
            signal,
            envData,
            "replan_exhausted",
            [
              `planner replanned ${failure.passes}× without committing to an acting workflow`,
            ],
            failure.attempts,
            trace,
          );
          return;
        case "execute":
          await reportExecuteFailure(
            signal,
            failure.error,
            failure.reason,
            failure.stepIndex,
            trace,
          );
          return;
      }
    },

    async handleCrash(signal, err, trace) {
      await reportExecuteFailure(signal, err, undefined, undefined, trace);
    },
  };
}

function buildPromptPrefix(sessionContext: string, envContext: string | null): string {
  return envContext ? `${sessionContext}\n\n---\n\n${envContext}` : sessionContext;
}

// Compiler couldn't produce a valid workflow — degrade to the pre-workflow
// path: spawn an AgentLoop with the source-matched skill and the engine's
// usual meta-skills. Preserves the user-facing behaviour we had before
// the workflow-mode switch.
async function runFallbackAgentLoop(
  engine: Engine,
  signal: PendingSignal,
  envData: EnvData,
  trace: Trace,
): Promise<void> {
  const sessionContext = buildSessionContext(envData);

  let loop;
  try {
    loop = await engine.startAgentLoop({
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
      `[supervisor] signal #${signal.id} fallback agent-loop start failed: ${(err as Error).message}`,
    );
    return;
  }
  loop.messages.push({ role: "user", content: signal.content });

  try {
    await loop.run();
  } catch (err) {
    console.error(`[supervisor] fallback agent-loop ${loop.id} crashed:`, err);
    await reportFailureToUser(engine, signal, loop.messages, err, trace).catch(
      (err2) => {
        console.error(`[supervisor] recovery also failed:`, err2);
      },
    );
  } finally {
    engine.endAgentLoop(loop.id);
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

// Cap on the transcript JSON included in a recovery briefing. The crashed
// buffer can hold huge tool results; recovery runs on the cheap `base`
// preset, and overflowing ITS context would make the recovery crash too.
// The tail is what matters (the messages around the failure).
const RECOVERY_LOG_MAX_CHARS = 20_000;

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
  const fullLog = JSON.stringify(failedMessages, null, 2);
  const log =
    fullLog.length > RECOVERY_LOG_MAX_CHARS
      ? `… (${fullLog.length - RECOVERY_LOG_MAX_CHARS} chars truncated)\n${fullLog.slice(-RECOVERY_LOG_MAX_CHARS)}`
      : fullLog;
  const briefing = `Error: ${errMsg}\n\nMessage log (tail):\n${log}`;
  await spawnRecovery(engine, signal, briefing, trace);
}

async function spawnRecovery(
  engine: Engine,
  signal: PendingSignal,
  briefing: string,
  trace: Trace,
): Promise<void> {
  const loop = await engine.startAgentLoop({
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
  loop.messages.push({ role: "user", content: briefing });

  try {
    await loop.run();
  } finally {
    engine.endAgentLoop(loop.id);
  }
}
