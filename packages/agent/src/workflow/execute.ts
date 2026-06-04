import type OpenAI from "openai";
import type { ChatCompletionMessageParam } from "openai/resources/chat/completions";
import type { ModelPreset, PresetName } from "../models";
import type { SessionOpts } from "../session";
import { SET_MEMORY_TOOL_NAME, SetMemoryArgsSchema } from "../synthetic-tools";
import type { Span, SpanKind, TraceContext } from "../tracing";
import type {
  LlmAgentStep,
  LlmComposeStep,
  ParallelStep,
  Step,
  ToolStep,
  Workflow,
} from "./dsl";
import {
  createStore,
  DuplicateBindingError,
  MissingBindingError,
  substitute,
  type VariableStore,
} from "./variables";

// Workflow execution runtime. Given a validated `Workflow`, walk its steps
// in order and execute each one against the engine. Variable store is
// opt-in: each step declares what bindings it needs in `input`/`args`,
// the executor resolves `${path}` placeholders, and binds the step's
// result back into the store under `bind`.
//
// What the executor does NOT do:
//   - decide whether to fall back to agentic mode (caller decides on
//     ExecResult failure)
//   - emit prompts itself (the compiler produced the workflow; the
//     executor just runs it)
//   - touch conversation history of any previous session (each
//     llm_compose is a fresh API call; llm_agent spawns a fresh Session)

export type ExecFailureReason =
  | "step_failed"
  | "missing_binding"
  | "duplicate_binding"
  | "skill_not_found"
  | "tool_error"
  | "llm_error";

export type ExecResult =
  | { ok: true; store: VariableStore }
  | {
      ok: false;
      reason: ExecFailureReason;
      error: Error;
      stepIndex: number;
      step: Step;
      store: VariableStore;
    };

export interface ExecContext {
  store: VariableStore;
  // Caller-provided trace scope. Executor opens its own root span inside
  // for the whole workflow, and per-step children inside that.
  parentTrace: TraceContext;
  // Free-form label used for log lines and the sub-session id prefix
  // on `llm_agent` spawns. Pass `${signal.source}:${signal.id}` to
  // match the existing supervisor convention.
  signalLabel: string;
}

export interface Executor {
  execute(workflow: Workflow, ctx: ExecContext): Promise<ExecResult>;
}

// Executor depends on only this subset of the engine's surface. The real
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

// Surface of Session that the executor touches when running an
// `llm_agent` step. The real Session has many more methods/fields — we
// only need these two.
export interface SubSessionHandle {
  messages: ChatCompletionMessageParam[];
  run(): Promise<string>;
}

export interface ExecutorDeps {
  engine: EngineSurface;
  // Decoupled from skills.ts so tests can pass a stub. Returns the
  // skill body (no frontmatter); null when not found.
  readSkill: (name: string) => Promise<string | null>;
  // Agent-side memory KV writer (agent.db). A `set_memory` tool step is
  // dispatched here, NOT to MCP — set_memory is a synthetic agent-side
  // tool with no MCP counterpart. Injected (rather than imported) so the
  // executor stays decoupled from db/memory and tests can spy on it.
  setMemory: (key: string, value: string) => void;
}

export function createExecutor(deps: ExecutorDeps): Executor {
  return {
    async execute(workflow, ctx) {
      const rootSpan = ctx.parentTrace.span({
        // Span name kept as "runner" for trace continuity with
        // pre-rename Langfuse history — do not change to "execute".
        name: "runner",
        kind: "chain",
        input: { stepCount: workflow.steps.length },
        metadata: { workflow_version: workflow.version },
      });

      try {
        for (let i = 0; i < workflow.steps.length; i++) {
          const step = workflow.steps[i]!;
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

        // No explicit terminal — that's fine, workflow ran to end of list.
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
  | { ok: false; reason: ExecFailureReason; error: Error };

async function runOneStep(
  step: Step,
  index: number,
  store: VariableStore,
  parent: TraceContext,
  ctx: ExecContext,
  deps: ExecutorDeps,
): Promise<StepOutcome> {
  const span = parent.span({
    name: `step[${index}]:${step.kind}`,
    kind: stepSpanKind(step.kind),
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

// Trace observation kind per step kind, so each renders with the right
// badge in the UI: a tool step IS a tool call; an llm_agent step spawns a
// sub-agent; llm_compose / parallel are multi-part links in the chain.
function stepSpanKind(kind: Step["kind"]): SpanKind {
  switch (kind) {
    case "tool":
      return "tool";
    case "llm_agent":
      return "agent";
    case "llm_compose":
    case "parallel":
      return "chain";
    case "terminal":
      return "span";
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

function classifyError(err: Error): ExecFailureReason {
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
  ctx: ExecContext,
  deps: ExecutorDeps,
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
  deps: ExecutorDeps,
): Promise<void> {
  const resolvedArgs = substitute(step.args, store) as Record<string, unknown>;
  span.update({ input: { tool: step.tool, args: resolvedArgs } });

  // Agent-side builtins are handled in-process, never forwarded to MCP.
  // Currently just set_memory (writes the agent.db memory KV) — workflows
  // need it for watermark updates (e.g. news_digest.last_read_at). It is a
  // synthetic tool with no MCP counterpart, so routing it to mcp.callTool
  // would fail with "unknown tool".
  if (step.tool === SET_MEMORY_TOOL_NAME) {
    const out = execSetMemory(resolvedArgs, deps);
    if (step.bind) store.set(step.bind, out);
    span.update({ output: out });
    return;
  }

  let raw: string;
  try {
    raw = await deps.engine.mcp.callTool(step.tool, resolvedArgs);
  } catch (err) {
    throw new ToolCallError(step.tool, (err as Error).message);
  }

  // MCP error responses come back as text starting with `[tool error]`.
  // Surface those as ToolCallError so the executor classifies correctly.
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

// set_memory — the one synthetic agent-side tool reachable as a direct
// workflow step (watermark writes, e.g. news_digest.last_read_at). Same
// validation as Session.applySetMemory; on bad args we throw ToolCallError
// so the executor classifies it as a tool failure like any other step.
// The other synthetic tools stay agentic-only: invoke_sub_agent is
// superseded by the `llm_agent` step kind, and skill read/write is the
// agentic `dreaming` flow's job.
function execSetMemory(
  args: Record<string, unknown>,
  deps: ExecutorDeps,
): { ok: true; key: string } {
  const parsed = SetMemoryArgsSchema.safeParse(args);
  if (!parsed.success) {
    const detail = parsed.error.issues
      .map((i) => `${i.path.join(".") || "args"}: ${i.message}`)
      .join("; ");
    throw new ToolCallError("set_memory", detail);
  }
  deps.setMemory(parsed.data.key, parsed.data.value);
  return { ok: true, key: parsed.data.key };
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
  deps: ExecutorDeps,
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
  ctx: ExecContext,
  deps: ExecutorDeps,
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
    // not under the executor root — keeps trace tree readable.
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
  ctx: ExecContext,
  deps: ExecutorDeps,
): Promise<void> {
  await Promise.all(
    step.steps.map(async (child, i) => {
      const childSpan = span.span({
        name: `parallel[${i}]:${child.kind}`,
        kind: stepSpanKind(child.kind),
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

// Re-export the store factory for callers (the workflow facade) that
// need to seed env/signal into the variable store before execute().
export { createStore };
