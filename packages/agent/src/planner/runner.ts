import type OpenAI from "openai";
import type { ChatCompletionMessageParam } from "openai/resources/chat/completions";
import type { ModelPreset, PresetName } from "../models";
import type { SessionOpts } from "../session";
import type { Span, TraceContext } from "../tracing";
import type {
  LlmAgentStep,
  LlmComposeStep,
  ParallelStep,
  Plan,
  Step,
  ToolStep,
} from "./dsl";
import {
  createStore,
  DuplicateBindingError,
  MissingBindingError,
  substitute,
  type VariableStore,
} from "./substitute";

// Plan-then-execute runtime. Given a validated `Plan`, walk its steps
// in order and execute each one against the engine. Variable store is
// opt-in: each step declares what bindings it needs in `input`/`args`,
// the runner resolves `${path}` placeholders, and binds the step's
// result back into the store under `bind`.
//
// What runner does NOT do:
//   - decide whether to fall back to agentic mode (caller decides on
//     RunFailure)
//   - emit prompts itself (planner produced the plan; runner just runs it)
//   - touch conversation history of any previous session (each
//     llm_compose is a fresh API call; llm_agent spawns a fresh Session)

export type RunFailureReason =
  | "step_failed"
  | "missing_binding"
  | "duplicate_binding"
  | "skill_not_found"
  | "tool_error"
  | "llm_error";

export type RunResult =
  | { ok: true; store: VariableStore }
  | {
      ok: false;
      reason: RunFailureReason;
      error: Error;
      stepIndex: number;
      step: Step;
      store: VariableStore;
    };

export interface RunContext {
  store: VariableStore;
  // Caller-provided trace scope. Runner opens its own root span inside
  // for the whole plan, and per-step children inside that.
  parentTrace: TraceContext;
  // Free-form label used for log lines and the sub-session id prefix
  // on `llm_agent` spawns. Pass `${signal.source}:${signal.id}` to
  // match the existing supervisor convention.
  signalLabel: string;
}

export interface Runner {
  run(plan: Plan, ctx: RunContext): Promise<RunResult>;
}

// Runner depends on only this subset of the engine's surface. The real
// `Engine` class structurally matches; test mocks can be plain objects
// without faking the full Engine constructor / private state. This
// avoids `as unknown as Engine` casts in tests.
export interface EngineSurface {
  readonly presets: Record<PresetName, ModelPreset>;
  resolveProvider(model: string): {
    client: OpenAI;
    kind: "deepseek" | "openai";
  };
  mcp: {
    callTool(name: string, args: Record<string, unknown>): Promise<string>;
  };
  startSession(opts: SessionOpts): Promise<SubSessionHandle>;
  endSession(id: string): void;
}

// Surface of Session that runner touches when running an `llm_agent`
// step. The real Session has many more methods/fields — we only need
// these two.
export interface SubSessionHandle {
  messages: ChatCompletionMessageParam[];
  run(): Promise<string>;
}

export interface RunnerDeps {
  engine: EngineSurface;
  // Decoupled from skills.ts so tests can pass a stub. Returns the
  // skill body (no frontmatter); null when not found.
  readSkill: (name: string) => Promise<string | null>;
}

export function createRunner(deps: RunnerDeps): Runner {
  return {
    async run(plan, ctx) {
      const rootSpan = ctx.parentTrace.span({
        name: "runner",
        input: { stepCount: plan.steps.length },
        metadata: { plan_version: plan.version },
      });

      try {
        for (let i = 0; i < plan.steps.length; i++) {
          const step = plan.steps[i]!;
          const outcome = await runOneStep(step, i, ctx.store, rootSpan, ctx, deps);
          if (!outcome.ok) {
            rootSpan.end({
              level: "ERROR",
              statusMessage: outcome.error.message,
              output: { failedAtIndex: i, reason: outcome.reason },
            });
            return {
              ok: false,
              reason: outcome.reason,
              error: outcome.error,
              stepIndex: i,
              step,
              store: ctx.store,
            };
          }
          if (outcome.stop) {
            rootSpan.end({ output: { stoppedAtIndex: i, stoppedBy: step.kind } });
            return { ok: true, store: ctx.store };
          }
        }

        // No explicit terminal — that's fine, plan ran to end of list.
        rootSpan.end({ output: { ranToEnd: true } });
        return { ok: true, store: ctx.store };
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        rootSpan.end({ level: "ERROR", statusMessage: message });
        throw err;
      }
    },
  };
}

// ─── per-step execution ──────────────────────────────────────────────

type StepOutcome =
  | { ok: true; stop: boolean }
  | { ok: false; reason: RunFailureReason; error: Error };

async function runOneStep(
  step: Step,
  index: number,
  store: VariableStore,
  parent: TraceContext,
  ctx: RunContext,
  deps: RunnerDeps,
): Promise<StepOutcome> {
  const span = parent.span({
    name: `step[${index}]:${step.kind}`,
    metadata: stepMetadata(step),
  });
  try {
    const stop = await dispatch(step, store, span, ctx, deps);
    span.end({ output: stop ? { stop: true } : { ok: true } });
    return { ok: true, stop };
  } catch (err) {
    const error = err instanceof Error ? err : new Error(String(err));
    const reason = classifyError(error);
    span.end({
      level: "ERROR",
      statusMessage: error.message,
      output: { reason },
    });
    return { ok: false, reason, error };
  }
}

function stepMetadata(step: Step): Record<string, unknown> {
  switch (step.kind) {
    case "tool":
      return { tool: step.tool, bind: step.bind ?? null };
    case "llm_compose":
      return {
        preset: step.preset,
        skill: step.skill ?? null,
        bind: step.bind,
      };
    case "llm_agent":
      return {
        preset: step.preset,
        skill: step.skill,
        bind: step.bind,
        tool_count: step.tools.length,
        max_iterations: step.maxIterations,
      };
    case "parallel":
      return { child_count: step.steps.length };
    case "terminal":
      return {};
  }
}

function classifyError(err: Error): RunFailureReason {
  if (err instanceof MissingBindingError) return "missing_binding";
  if (err instanceof DuplicateBindingError) return "duplicate_binding";
  if (err.name === "SkillNotFoundError") return "skill_not_found";
  if (err.name === "ToolCallError") return "tool_error";
  if (err.name === "LlmCallError") return "llm_error";
  return "step_failed";
}

async function dispatch(
  step: Step,
  store: VariableStore,
  span: Span,
  ctx: RunContext,
  deps: RunnerDeps,
): Promise<boolean> {
  switch (step.kind) {
    case "tool":
      await execTool(step, store, span, deps);
      return false;
    case "llm_compose":
      await execLlmCompose(step, store, span, deps);
      return false;
    case "llm_agent":
      await execLlmAgent(step, store, span, ctx, deps);
      return false;
    case "parallel":
      await execParallel(step, store, span, ctx, deps);
      return false;
    case "terminal":
      return true;
  }
}

// ─── tool ────────────────────────────────────────────────────────────

class ToolCallError extends Error {
  constructor(public readonly tool: string, message: string) {
    super(`tool ${tool} failed: ${message}`);
    this.name = "ToolCallError";
  }
}

async function execTool(
  step: ToolStep,
  store: VariableStore,
  span: Span,
  deps: RunnerDeps,
): Promise<void> {
  const resolvedArgs = substitute(step.args, store) as Record<string, unknown>;
  span.update({ input: { tool: step.tool, args: resolvedArgs } });

  let raw: string;
  try {
    raw = await deps.engine.mcp.callTool(step.tool, resolvedArgs);
  } catch (err) {
    throw new ToolCallError(step.tool, (err as Error).message);
  }

  // MCP error responses come back as text starting with `[tool error]`.
  // Surface those as ToolCallError so the runner classifies correctly.
  if (raw.startsWith("[tool error]")) {
    throw new ToolCallError(step.tool, raw);
  }

  const parsed = tryParseJson(raw);
  if (step.bind) {
    store.set(step.bind, parsed);
  }
  span.update({ output: parsed });
}

function tryParseJson(raw: string): unknown {
  if (!raw) return raw;
  const first = raw[0];
  if (first !== "{" && first !== "[") return raw;
  try {
    return JSON.parse(raw);
  } catch {
    return raw;
  }
}

// ─── llm_compose ─────────────────────────────────────────────────────

class SkillNotFoundError extends Error {
  constructor(public readonly skill: string) {
    super(`skill not found: ${skill}`);
    this.name = "SkillNotFoundError";
  }
}

class LlmCallError extends Error {
  constructor(message: string) {
    super(`llm call failed: ${message}`);
    this.name = "LlmCallError";
  }
}

async function execLlmCompose(
  step: LlmComposeStep,
  store: VariableStore,
  span: Span,
  deps: RunnerDeps,
): Promise<void> {
  const preset = deps.engine.presets[step.preset];
  const provider = deps.engine.resolveProvider(preset.model);

  let system: string | undefined;
  if (step.skill) {
    const body = await deps.readSkill(step.skill);
    if (body === null) throw new SkillNotFoundError(step.skill);
    system = body;
  }

  const resolvedInput: Record<string, unknown> = {};
  for (const [k, v] of Object.entries(step.input)) {
    resolvedInput[k] = substitute(v, store);
  }
  const userPrompt = step.prompt
    ? (substitute(step.prompt, store) as string)
    : "";
  const inputBlocks = renderInputAsXml(resolvedInput);
  const userText = userPrompt
    ? inputBlocks
      ? `${userPrompt}\n\n${inputBlocks}`
      : userPrompt
    : inputBlocks;

  const messages: ChatCompletionMessageParam[] = [];
  if (system) messages.push({ role: "system", content: system });
  messages.push({ role: "user", content: userText });

  const body = buildLlmComposeBody(preset, provider.kind, messages);

  const gen = span.generation({
    name: `llm_compose:${step.bind}`,
    model: preset.model,
    modelParameters: {
      reasoning_effort: preset.reasoningEffort,
      tools_mode: "none",
    },
    input: messages,
  });

  let content: string;
  let usage: { input: number; output: number; total: number } | undefined;
  try {
    const response = await provider.client.chat.completions.create(body);
    content = response.choices[0]?.message.content ?? "";
    const u = response.usage;
    if (u) {
      usage = {
        input: u.prompt_tokens,
        output: u.completion_tokens,
        total: u.total_tokens,
      };
    }
  } catch (err) {
    const message = err instanceof Error ? err.message : String(err);
    gen.end({ output: { error: message }, level: "ERROR", statusMessage: message });
    throw new LlmCallError(message);
  }

  gen.end({ output: content, usage });
  store.set(step.bind, content);
}

function buildLlmComposeBody(
  preset: ModelPreset,
  providerKind: "deepseek" | "openai",
  messages: ChatCompletionMessageParam[],
) {
  const base = {
    model: preset.model,
    messages,
    // tools=[] would be ideal, but the OpenAI SDK rejects an empty
    // array on chat.completions.create. Omitting the field has the
    // same effect: no tools declared = no tool_calls possible.
  };
  if (providerKind === "deepseek") {
    return {
      ...base,
      ...(preset.reasoningEffort === "disabled"
        ? { thinking: { type: "disabled" as const } }
        : {
            thinking: { type: "enabled" as const },
            reasoning_effort: preset.reasoningEffort,
          }),
    };
  }
  return base;
}

// XML-style input blocks render reliably for both OpenAI and DeepSeek
// (Anthropic's "use tags" recommendation transfers in practice — the
// models lock onto tag boundaries better than they do JSON-dump
// boundaries). Empty input → empty string, no block noise.
function renderInputAsXml(input: Record<string, unknown>): string {
  const entries = Object.entries(input);
  if (entries.length === 0) return "";
  const blocks = entries.map(([k, v]) => {
    const rendered =
      typeof v === "string" ? v : JSON.stringify(v, null, 2);
    return `<${k}>\n${rendered}\n</${k}>`;
  });
  return blocks.join("\n\n");
}

// ─── llm_agent ───────────────────────────────────────────────────────

async function execLlmAgent(
  step: LlmAgentStep,
  store: VariableStore,
  span: Span,
  ctx: RunContext,
  deps: RunnerDeps,
): Promise<void> {
  const prompt = substitute(step.prompt, store) as string;
  const allowedTools = new Set(step.tools);
  const childId = `${ctx.signalLabel}__agent:${step.bind}`;

  const child = await deps.engine.startSession({
    id: childId,
    skills: [step.skill],
    includeEngineSkills: false,
    preset: step.preset,
    maxIterations: step.maxIterations,
    parentId: ctx.signalLabel,
    toolWhitelist: allowedTools,
    // Nest the sub-session's iters/tool spans under THIS step's span,
    // not under the runner root — keeps trace tree readable.
    traceScope: span,
  });

  child.messages.push({ role: "user", content: prompt });

  let result: string;
  try {
    result = await child.run();
  } finally {
    deps.engine.endSession(childId);
  }

  store.set(step.bind, result);
  span.update({ output: result });
}

// ─── parallel ────────────────────────────────────────────────────────

async function execParallel(
  step: ParallelStep,
  store: VariableStore,
  span: Span,
  ctx: RunContext,
  deps: RunnerDeps,
): Promise<void> {
  await Promise.all(
    step.steps.map(async (child, i) => {
      const childSpan = span.span({
        name: `parallel[${i}]:${child.kind}`,
        metadata: stepMetadata(child),
      });
      try {
        await dispatch(child, store, childSpan, ctx, deps);
        childSpan.end({ output: { ok: true } });
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        childSpan.end({ level: "ERROR", statusMessage: message });
        throw err;
      }
    }),
  );
}

// ─── shared helpers exported for the executor's own tests ────────────

export const __testing = {
  renderInputAsXml,
  buildLlmComposeBody,
  classifyError,
  ToolCallError,
  SkillNotFoundError,
  LlmCallError,
};

// Re-export the store factory for callers (supervisor) that need to
// seed env/signal into the variable store before run().
export { createStore };
