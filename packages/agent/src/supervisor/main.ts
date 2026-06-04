import "dotenv/config";
import { createEngine, type Engine } from "../engine";
import { DEFAULT_PRESETS } from "../models";
import { gatherEnvData } from "../session-context";
import { listSkills, readSkill } from "../skills";
import { createWorkflow, type Workflow } from "../workflow";
import {
  createFallback,
  type Fallback,
  type PendingSignal,
  type WorkflowFailure,
} from "./fallback";

// Long-running supervisor. The agent has no signal sources of its own —
// every external event (Telegram, Gmail, cron, webhook) lives inside the
// MCP server, which queues signals into its own DB. Each signal flows:
//
//   signal → workflow (compile → execute)
//          ↳ compile failed → fallback: agentic Session
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
  fallback: Fallback,
): Promise<void> {
  const signalLabel = `${signal.source}:${signal.id}`;
  const envData = await gatherEnvData(engine);

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
    const result = await workflow.runForSignal(signal, envData, trace, signalLabel);

    if (!result.ok) {
      let failure: WorkflowFailure;
      if (result.stage === "compile") {
        failure = {
          stage: "compile",
          reason: result.reason,
          errors: result.errors,
          attempts: result.attempts,
        };
      } else if (result.stage === "replan_exhausted") {
        // The planner replanned on every pass without committing. Treat it
        // like a compile miss — degrade to an agentic session.
        failure = {
          stage: "compile",
          reason: "replan_exhausted",
          errors: [
            `planner replanned ${result.passes}× without committing to an acting workflow`,
          ],
          attempts: result.attempts,
        };
      } else {
        failure = {
          stage: "execute",
          reason: result.reason,
          stepIndex: result.stepIndex,
          error: result.error,
        };
      }
      // Timeline marker for the workflow→agentic handoff. A compile miss is
      // an expected degradation (WARNING); an execute failure means side
      // effects may already have fired (ERROR).
      trace.event({
        name: "fallback",
        level: failure.stage === "compile" ? "WARNING" : "ERROR",
        metadata:
          failure.stage === "compile"
            ? { stage: "compile", reason: failure.reason, attempts: failure.attempts }
            : {
                stage: "execute",
                reason: failure.reason,
                step_index: failure.stepIndex,
                error:
                  failure.error instanceof Error
                    ? failure.error.message
                    : String(failure.error),
              },
      });
      await fallback.handle(signal, envData, failure, trace);
      return;
    }

    console.log(
      `[supervisor] signal #${signal.id} workflow ok (attempts=${result.attempts}, steps=${result.stepCount})`,
    );
  } catch (err) {
    console.error(`[supervisor] signal #${signal.id} unexpected error:`, err);
    trace.event({
      name: "fallback",
      level: "ERROR",
      metadata: {
        stage: "execute",
        error: err instanceof Error ? err.message : String(err),
      },
    });
    await fallback.handle(signal, envData, { stage: "execute", error: err }, trace);
  } finally {
    trace.end();
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
  // compiler role; current agentic sessions never select it.
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
    // session path now; workflow-mode signals never load these.
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

  const fallback = createFallback({ engine });

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

  console.log("[supervisor] entering main loop (workflow-mode)");
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
      await runSignal(engine, result.signal, workflow, fallback);
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
