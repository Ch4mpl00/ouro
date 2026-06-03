import type { ChatCompletionTool } from "openai/resources/chat/completions";
import { setMemory } from "../db/memory";
import type { EnvData } from "../session-context";
import { SET_MEMORY_TOOL } from "../synthetic-tools";
import type { TraceContext } from "../tracing";
import {
  createCompiler,
  type CompilerEngineSurface,
  type CompilerFailureReason,
} from "./compile";
import type { Step } from "./dsl";
import {
  createExecutor,
  type EngineSurface,
  type ExecFailureReason,
} from "./execute";
import { createStore, type VariableStore } from "./variables";

// Dynamic-workflow facade. The two halves of the mechanism — the
// compiler (LLM turns a signal into a validated Workflow) and the
// executor (runtime walks the steps) — are composed here so the
// supervisor depends on ONE surface, not on `compile` / `execute`
// directly.
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
    };

export interface Workflow {
  runForSignal(
    signal: WorkflowSignal,
    envData: EnvData,
    parentTrace: TraceContext,
    signalLabel: string,
  ): Promise<WorkflowRunResult>;
}

export interface WorkflowDeps {
  engine: CompilerEngineSurface & EngineSurface;
  // Full MCP tool definitions — feed the compiler's schema enums and
  // rendered tool signatures (see compile.ts).
  mcpTools: readonly ChatCompletionTool[];
  // Skill names the compiler may emit, and the loader the executor uses
  // for `llm_compose` / `llm_agent` skills. Returns the skill body (no
  // frontmatter), or null when not found.
  knownSkills: readonly string[];
  readSkill: (name: string) => Promise<string | null>;
  // Compiler retry budget (initial attempt + retries). Default 3.
  maxAttempts?: number;
}

export function createWorkflow(deps: WorkflowDeps): Workflow {
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
    setMemory,
  });

  return {
    async runForSignal(signal, envData, parentTrace, signalLabel) {
      const compiled = await compiler.compile({
        signal: {
          source: signal.source,
          content: signal.content,
          envContext: signal.envContext,
        },
        envData,
        parentTrace,
        signalLabel,
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

      // Seed the variable store with env + signal context. Steps see
      // only the bindings they name via `${path}` placeholders.
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

      return {
        ok: true,
        attempts: compiled.attempts,
        stepCount: compiled.workflow.steps.length,
        store: executed.store,
      };
    },
  };
}
