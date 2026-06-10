import type { ChatCompletionTool } from "openai/resources/chat/completions";
import type { EnvData } from "../session-context";
import { SET_MEMORY_TOOL } from "../synthetic-tools";
import type { TraceContext } from "../tracing";
import {
  createCompiler,
  type CompilerEngineSurface,
  type CompilerFailureReason,
  type PriorContext,
} from "./compile";
import type { Step } from "./dsl";
import {
  createExecutor,
  type EngineSurface,
  type ExecFailureReason,
} from "./execute";
import { createStore, type VariableStore } from "./variables";

// Dynamic-workflow runner. The two halves of the mechanism — the
// compiler (LLM turns a signal into a validated Workflow) and the
// executor (runtime walks the steps) — are composed here so the
// supervisor depends on ONE surface, not on `compile` / `execute`
// directly. ("Workflow" the data type — the compiled plan — lives in
// ./dsl; this module is the RUNNER that produces and executes plans.)
//
//   runForSignal:  signal → compile → execute → result
//
// The result is a discriminated union so the caller can route failures:
// a `compile` failure degrades to an agentic fallback session; an
// `execute` failure means side effects may have already fired, so the
// caller reports the failure to the user instead.

export interface WorkflowSignal {
  id: number;
  source: string;
  content: string;
  envContext: string | null;
}

export type WorkflowRunResult =
  | { ok: true; attempts: number; stepCount: number; store: VariableStore }
  | {
      ok: false;
      stage: "compile";
      reason: CompilerFailureReason;
      errors: string[];
      attempts: number;
    }
  | {
      ok: false;
      stage: "execute";
      reason: ExecFailureReason;
      error: Error;
      stepIndex: number;
      step: Step;
    }
  // The planner emitted `replan` on every pass without ever committing to
  // an acting workflow. Treated like a compile failure by the supervisor
  // (degrade to an agentic session) — it means the planner couldn't
  // converge on a plan.
  | { ok: false; stage: "replan_exhausted"; passes: number; attempts: number };

// The failure half of the union — what the fallback module consumes.
export type WorkflowRunFailure = Exclude<WorkflowRunResult, { ok: true }>;

export interface WorkflowRunner {
  runForSignal(
    signal: WorkflowSignal,
    envData: EnvData,
    parentTrace: TraceContext,
  ): Promise<WorkflowRunResult>;
}

export interface WorkflowRunnerDeps {
  engine: CompilerEngineSurface & EngineSurface;
  // Full MCP tool definitions — feed the compiler's schema enums and
  // rendered tool signatures (see compile.ts).
  mcpTools: readonly ChatCompletionTool[];
  // Skill names the compiler may emit, and the loader the executor uses
  // for `llm_compose` / `llm_agent` skills. Returns the skill body (no
  // frontmatter), or null when not found.
  knownSkills: readonly string[];
  readSkill: (name: string) => Promise<string | null>;
  // Agent-side memory KV writer — dispatched by the executor for
  // `set_memory` tool steps (watermark writes). Injected by the
  // composition root, same instance the AgentLoop path uses.
  setMemory: (key: string, value: string) => void;
  // Compiler retry budget (initial attempt + retries). Default 3.
  maxAttempts?: number;
  // Total planning passes per signal: the initial plan plus replans.
  // Default 3 (initial + up to 2 replans). Exceeding it is a failure.
  maxPasses?: number;
}

export function createWorkflowRunner(deps: WorkflowRunnerDeps): WorkflowRunner {
  const compiler = createCompiler({
    engine: deps.engine,
    readSkill: deps.readSkill,
    // set_memory is a synthetic agent-side tool with no MCP counterpart;
    // surface it to the compiler too so it appears in the schema enum and
    // the prompt's tool signatures (the executor dispatches it directly).
    mcpTools: [...deps.mcpTools, SET_MEMORY_TOOL],
    knownSkills: deps.knownSkills,
    maxAttempts: deps.maxAttempts,
  });
  const executor = createExecutor({
    engine: deps.engine,
    readSkill: deps.readSkill,
    setMemory: deps.setMemory,
  });

  const maxPasses = deps.maxPasses ?? 3;

  return {
    async runForSignal(signal, envData, parentTrace) {
      const signalLabel = `${signal.source}:${signal.id}`;
      // The plan→act→replan loop. Pass 0 is the initial plan; each `replan`
      // step carries context into the next pass. Bounded by `maxPasses` so
      // a planner that never commits can't loop forever.
      let priorContext: PriorContext | undefined;
      let lastAttempts = 0;

      for (let pass = 0; pass < maxPasses; pass++) {
        const compiled = await compiler.compile({
          signal: {
            source: signal.source,
            content: signal.content,
            envContext: signal.envContext,
          },
          envData,
          parentTrace,
          signalLabel,
          priorContext,
        });

        if (!compiled.ok) {
          return {
            ok: false,
            stage: "compile",
            reason: compiled.reason,
            errors: compiled.errors,
            attempts: compiled.attempts,
          };
        }
        lastAttempts = compiled.attempts;

        // Seed the variable store with env + signal context, plus any
        // context carried from a prior replan pass (referenceable as
        // `${context.<name>}`). Steps see only the bindings they name.
        const store = createStore({
          env: {
            timezone: envData.timezone,
            now: envData.now.toISOString(),
            newsLastReadAt: envData.newsLastReadAt,
            userEmail: envData.userEmail,
          },
          signal: {
            source: signal.source,
            content: signal.content,
            id: signal.id,
          },
          ...(priorContext ? { context: priorContext.data } : {}),
        });

        const executed = await executor.execute(compiled.workflow, {
          store,
          parentTrace,
          signalLabel,
        });

        if (!executed.ok) {
          return {
            ok: false,
            stage: "execute",
            reason: executed.reason,
            error: executed.error,
            stepIndex: executed.stepIndex,
            step: executed.step,
          };
        }

        if (executed.replan) {
          // Carry context into the next pass. `lastPass` is true when the
          // pass that consumes this context is the final allowed one, so
          // the compiler prompt forces a commit there.
          priorContext = {
            pass: pass + 1,
            lastPass: pass + 1 === maxPasses - 1,
            data: executed.replan.context,
            note: executed.replan.note,
          };
          continue;
        }

        return {
          ok: true,
          attempts: compiled.attempts,
          stepCount: compiled.workflow.steps.length,
          store: executed.store,
        };
      }

      // Every pass asked to replan — the planner never committed.
      return {
        ok: false,
        stage: "replan_exhausted",
        passes: maxPasses,
        attempts: lastAttempts,
      };
    },
  };
}
