import type {
  ChatCompletionMessageParam,
  ChatCompletionTool,
} from "openai/resources/chat/completions";
import { z } from "zod";
import { zodToJsonSchema } from "zod-to-json-schema";
import { Cause, Effect, Exit } from "effect";
import {
  PRESET_NAMES,
  SET_MEMORY_TOOL,
  SET_MEMORY_TOOL_NAME,
  SetMemoryArgsSchema,
  appendPatch,
  runCodeAgent,
  runGeneration,
  toError,
  traceGenerationEffect,
  type AgentLoopOpts,
  type ChatProvider,
  type EnvData,
  type ModelPreset,
  type PresetName,
  type SessionContext,
} from "./agent-loop";
import type { CodexClient } from "./codex-client";
import { JUDGE_NODE_META } from "./trace-model";
import type { Generation, Span, SpanKind, Trace, TraceContext } from "./tracing";

// The dynamic-workflow module, end to end: the plan DSL, the variable store
// the plan binds into, the compiler that writes a plan from a signal, the
// runtime that walks it, and the facade the supervisor calls.
//
// Read top to bottom — each section only depends on the ones above it:
//
//   dsl          Workflow/Step shapes, the Zod schema built from the live
//                tool + skill registries, and `parseWorkflow`
//   variables    ${path} substitution and the bind/read store
//   compile      signal → validated Workflow, one LLM call plus retries
//   execute      the step runtime: tool, llm_compose, llm_agent, code_agent,
//                parallel, replan, terminal
//   runner       createWorkflowRunner — compile then execute, replan loop
//
// The agent runtime it sits on is ./agent-loop; ./codex-client, ./tracing
// and ./trace-model are the transport, observability and judge-tag surfaces.

// ═══════════════════════════════════════════════════════════════════
// DSL
// ═══════════════════════════════════════════════════════════════════

// Workflow DSL — the language the compiler LLM emits and the executor runs.
//
// Seven step kinds, no control flow primitives beyond `parallel`:
//
//   tool         — runtime → MCP tool with literal args, result bound
//   llm_compose  — LLM with no tools, structured output, result bound
//   llm_agent    — LLM with a bounded tool whitelist + maxIterations,
//                  internal ReAct loop, final text bound
//   code_agent   — delegate a computational/coding task to the Codex sandbox
//                  (writes + runs code), result bound
//   parallel     — flat list of independent leaf steps, run concurrently
//   terminal     — explicit end of workflow
//   replan       — terminator that bounces back to the compiler with the
//                  named bindings as context, for a fresh planning pass
//
// No `branch` / `if` / loops: empty-case handling lives inside
// llm_compose prompts. The data-dependent case — "I can't plan the rest
// until I see X" — is handled by `replan`: emit a gather workflow that
// ends in `replan`, and the runtime recompiles with what you gathered so
// the next pass can act. This is the structured, traced alternative to a
// ReAct sub-session.
//
// Context flow is opt-in: each llm_* step declares its `input`
// bindings explicitly. The runtime variable store persists across the
// workflow, but a step only sees the bindings it names. This is the
// inverse of the conversation-history model used by the current
// agentic supervisor.
//
// Substitution placeholders (`${path.to.value}`) appear in any string
// field. The Zod schema treats them as plain strings; the executor
// resolves them at runtime.
//
// Authoring constraint not encoded here: `output_schema`-style
// strict-mode emission requires further work to be OpenAI-strict
// compatible (record(unknown) → no `additionalProperties: false`).
// For Phase 1 we treat workflow validation as a server-side Zod check;
// strict-mode JSON schema is generated for inspection but Phase 2
// will likely use `json_object` mode + Zod retry-loop instead.

const PRESET_ENUM = z.enum(PRESET_NAMES as readonly [string, ...string[]]);

export interface WorkflowSchemaDeps {
  // Names from MCP tool registry, resolved at engine boot. The schema
  // rejects any tool the runtime doesn't know about — catches typos and
  // hallucinated names before execution.
  knownTools: readonly string[];
  // Names from the skill registry (live overlay ∪ shipped defaults).
  knownSkills: readonly string[];
}

export interface WorkflowSchemaBundle {
  WorkflowSchema: z.ZodTypeAny;
  // OpenAI `response_format: { type: "json_schema", strict: true }` body.
  // Phase 2 will probably consume this; Phase 1 just exposes it for
  // inspection during smoke-tests.
  workflowToJsonSchema: () => unknown;
}

export function createWorkflowSchema(deps: WorkflowSchemaDeps): WorkflowSchemaBundle {
  if (deps.knownTools.length === 0) {
    throw new Error("createWorkflowSchema: knownTools must be non-empty");
  }
  if (deps.knownSkills.length === 0) {
    throw new Error("createWorkflowSchema: knownSkills must be non-empty");
  }

  const ToolName = z.enum(
    deps.knownTools as readonly [string, ...string[]],
  );
  const SkillName = z.enum(
    deps.knownSkills as readonly [string, ...string[]],
  );

  const ToolStepSchema = z
    .object({
      kind: z.literal("tool"),
      tool: ToolName,
      args: z.record(z.unknown()),
      bind: z.string().min(1).optional(),
    })
    .strict();

  // No inline .refine() for "skill OR prompt required" — refine returns
  // ZodEffects, which z.discriminatedUnion can't accept as a member.
  // The check lives in postCheckWorkflow() instead and runs after the
  // discriminated-union pass succeeds.
  const LlmComposeStepSchema = z
    .object({
      kind: z.literal("llm_compose"),
      preset: PRESET_ENUM,
      skill: SkillName.optional(),
      prompt: z.string().min(1).optional(),
      input: z.record(z.unknown()),
      bind: z.string().min(1),
    })
    .strict();

  const LlmAgentStepSchema = z
    .object({
      kind: z.literal("llm_agent"),
      preset: PRESET_ENUM,
      skill: SkillName,
      prompt: z.string().min(1),
      tools: z.array(ToolName).min(1),
      maxIterations: z.number().int().min(1).max(20),
      bind: z.string().min(1),
    })
    .strict();

  // `code_agent` delegates a computational/coding task to the sandboxed Codex
  // agent (writes + runs code, returns the result). A distinct kind — not a
  // `tool` step — because it spawns a sub-agent like `llm_agent`, so it badges
  // as an agent in traces and takes a natural-language `task` rather than typed
  // tool args. `data` is optional stdin for the program.
  const CodeAgentStepSchema = z
    .object({
      kind: z.literal("code_agent"),
      task: z.string().min(1),
      data: z.string().optional(),
      bind: z.string().min(1),
    })
    .strict();

  const TerminalStepSchema = z
    .object({
      kind: z.literal("terminal"),
    })
    .strict();

  // `replan` is a terminator like `terminal`, but instead of ending the
  // signal it tells the runtime to recompile with `context` (the named
  // bindings) carried into the next pass. NOT a leaf step — it cannot
  // appear inside `parallel`.
  const ReplanStepSchema = z
    .object({
      kind: z.literal("replan"),
      // Bind names to carry into the next planning pass. The runtime seeds
      // them under `context.<name>` and renders them into the compiler's
      // prompt so the next pass can both read and reference them.
      context: z.array(z.string().min(1)).min(1),
      // Optional note from this pass to the next ("this is 'продолжай' — I
      // fetched the last 10 messages; decide what to continue and act").
      note: z.string().min(1).optional(),
    })
    .strict();

  // Leaf steps: the work-performing kinds allowed inside `parallel.steps`.
  // Nested `parallel` is intentionally forbidden (keeps traces readable,
  // keeps the compiler from over-engineering workflows). Terminators
  // (`terminal`, `replan`) are excluded too — the runtime ignores a stop
  // signal coming from a parallel branch, so allowing them in the schema
  // would let the compiler emit a step that silently does nothing.
  const LeafStepSchema = z.discriminatedUnion("kind", [
    ToolStepSchema,
    LlmComposeStepSchema,
    LlmAgentStepSchema,
    CodeAgentStepSchema,
  ]);

  const ParallelStepSchema = z
    .object({
      kind: z.literal("parallel"),
      steps: z.array(LeafStepSchema).min(2),
    })
    .strict();

  const StepSchema = z.discriminatedUnion("kind", [
    ToolStepSchema,
    LlmComposeStepSchema,
    LlmAgentStepSchema,
    CodeAgentStepSchema,
    TerminalStepSchema,
    ParallelStepSchema,
    ReplanStepSchema,
  ]);

  const WorkflowSchema = z
    .object({
      version: z.literal(1),
      steps: z.array(StepSchema).min(1),
    })
    .strict();

  return {
    WorkflowSchema,
    workflowToJsonSchema: () =>
      zodToJsonSchema(WorkflowSchema, {
        name: "Workflow",
        $refStrategy: "none",
      }),
  };
}

// ─── types (structural, not bound to a specific tool/skill list) ─────
//
// We could derive types from a concrete factory output via z.infer,
// but that ties consumers to a specific factory instance. Plain
// structural types let supervisor / executor code be written against
// the shape without threading the schema everywhere.

export interface ToolStep {
  kind: "tool";
  tool: string;
  args: Record<string, unknown>;
  bind?: string;
}

export interface LlmComposeStep {
  kind: "llm_compose";
  preset: PresetName;
  skill?: string;
  prompt?: string;
  input: Record<string, unknown>;
  bind: string;
}

export interface LlmAgentStep {
  kind: "llm_agent";
  preset: PresetName;
  skill: string;
  prompt: string;
  tools: string[];
  maxIterations: number;
  bind: string;
}

export interface CodeAgentStep {
  kind: "code_agent";
  task: string;
  data?: string;
  bind: string;
}

export interface ParallelStep {
  kind: "parallel";
  steps: LeafStep[];
}

export interface TerminalStep {
  kind: "terminal";
}

export interface ReplanStep {
  kind: "replan";
  context: string[];
  note?: string;
}

export type LeafStep = ToolStep | LlmComposeStep | LlmAgentStep | CodeAgentStep;
export type Step = LeafStep | TerminalStep | ParallelStep | ReplanStep;

export interface Workflow {
  version: 1;
  steps: Step[];
}

// ─── parse + error formatting ────────────────────────────────────────

export interface WorkflowParseSuccess {
  ok: true;
  workflow: Workflow;
}

export interface WorkflowParseFailure {
  ok: false;
  errors: string[];
}

export type WorkflowParseResult = WorkflowParseSuccess | WorkflowParseFailure;

export function parseWorkflow(
  input: unknown,
  schema: z.ZodTypeAny,
): WorkflowParseResult {
  const result = schema.safeParse(input);
  if (!result.success) {
    return { ok: false, errors: formatWorkflowErrors(result.error) };
  }
  const workflow = result.data as Workflow;
  const semanticErrors = postCheckWorkflow(workflow);
  if (semanticErrors.length > 0) {
    return { ok: false, errors: semanticErrors };
  }
  return { ok: true, workflow };
}

// Semantic checks that can't be expressed at Zod-schema level without
// breaking the discriminated union (e.g., constraints involving an
// either/or between two optional fields). Walks the workflow tree and
// returns one error string per violation, formatted with the same
// `at steps[N]...` convention as formatWorkflowErrors().
function postCheckWorkflow(workflow: Workflow): string[] {
  const errors: string[] = [];
  const visit = (step: Step, path: string): void => {
    if (step.kind === "llm_compose") {
      if (!step.skill && !step.prompt) {
        errors.push(
          `at ${path}: llm_compose requires either \`skill\` or \`prompt\` (or both)`,
        );
      }
    } else if (step.kind === "parallel") {
      step.steps.forEach((s, i) => visit(s, `${path}.steps[${i}]`));
    }
  };
  workflow.steps.forEach((s, i) => visit(s, `steps[${i}]`));
  return errors;
}

// Render Zod issues as one-line human-readable strings suitable for
// feeding back to the compiler LLM in a retry. Raw Zod issue trees are
// noisy and full of paths the model has to mentally parse; flattening
// to "step N kind=tool: <what's wrong>" gives much better first-retry
// success.
export function formatWorkflowErrors(error: z.ZodError): string[] {
  const out: string[] = [];
  for (const issue of error.issues) {
    const path = renderPath(issue.path);
    const where = path ? `at ${path}` : "at workflow root";
    out.push(`${where}: ${issue.message}`);
  }
  return out;
}

function renderPath(path: (string | number)[]): string {
  if (path.length === 0) return "";
  const parts: string[] = [];
  for (let i = 0; i < path.length; i++) {
    const seg = path[i];
    if (typeof seg === "number") {
      parts.push(`[${seg}]`);
    } else if (i === 0) {
      parts.push(String(seg));
    } else {
      parts.push(`.${seg}`);
    }
  }
  return parts.join("");
}

// ═══════════════════════════════════════════════════════════════════
// Variables
// ═══════════════════════════════════════════════════════════════════

// `${path}` substitution for workflow step args, llm_compose inputs, and
// llm_*  prompts. Two modes:
//
//   "whole string" — the string IS one placeholder, e.g. `"${posts}"`.
//     Returns the bound value AS-IS (preserves type). This is how an
//     array of post objects gets passed to a tool: caller writes
//     `args: { posts: "${posts}" }` and the tool sees an actual array,
//     not a stringified JSON dump.
//
//   "interpolation" — the string contains one or more placeholders
//     mixed with literal text. Each placeholder resolves and is
//     stringified (JSON.stringify for non-string values), then concat'd.
//     Used for prompts: `"Hello ${name}, today is ${env.date}"`.
//
// Missing bindings throw MissingBindingError — never silently expand
// to "undefined". Compiler mistakes should surface loudly in the trace.

export interface VariableStore {
  get(path: string): unknown;
  set(name: string, value: unknown): void;
  has(path: string): boolean;
  snapshot(): Record<string, unknown>;
}

export class MissingBindingError extends Error {
  constructor(public readonly path: string) {
    super(`unbound substitution: \${${path}}`);
    this.name = "MissingBindingError";
  }
}

export class DuplicateBindingError extends Error {
  constructor(public readonly name: string) {
    super(`duplicate binding: ${name}`);
    this.name = "DuplicateBindingError";
  }
}

// Dot-notation only for v1. No array indices (`results[0]`), no
// optional chaining, no expressions. If we need them, add in the
// minimal form when the first hot-path case appears.
const FULL_PLACEHOLDER = /^\$\{([^}]+)\}$/;
const PARTIAL_PLACEHOLDER = /\$\{([^}]+)\}/g;

export function createStore(initial: Record<string, unknown>): VariableStore {
  const data = new Map<string, unknown>(Object.entries(initial));

  const resolvePath = (path: string): { found: boolean; value: unknown } => {
    const segments = path.split(".");
    const first = segments[0] ?? "";
    if (!data.has(first)) return { found: false, value: undefined };
    let current: unknown = data.get(first);
    for (let i = 1; i < segments.length; i++) {
      const seg = segments[i] ?? "";
      if (current == null || typeof current !== "object") {
        return { found: false, value: undefined };
      }
      const obj = current as Record<string, unknown>;
      if (!(seg in obj)) return { found: false, value: undefined };
      current = obj[seg];
    }
    return { found: true, value: current };
  };

  return {
    get(path) {
      const r = resolvePath(path);
      if (!r.found) throw new MissingBindingError(path);
      return r.value;
    },
    set(name, value) {
      if (data.has(name)) throw new DuplicateBindingError(name);
      data.set(name, value);
    },
    has(path) {
      return resolvePath(path).found;
    },
    snapshot() {
      return Object.fromEntries(data);
    },
  };
}

// Recursively walk a value substituting `${path}` placeholders. Returns
// a NEW value (no in-place mutation) — caller can hold onto the input
// safely. Non-string primitives pass through untouched.
export function substitute(value: unknown, store: VariableStore): unknown {
  if (typeof value === "string") return substituteString(value, store);
  if (Array.isArray(value)) return value.map((v) => substitute(v, store));
  if (value && typeof value === "object") {
    const out: Record<string, unknown> = {};
    for (const [k, v] of Object.entries(value)) {
      out[k] = substitute(v, store);
    }
    return out;
  }
  return value;
}

function substituteString(s: string, store: VariableStore): unknown {
  const fullMatch = s.match(FULL_PLACEHOLDER);
  if (fullMatch) {
    return store.get(fullMatch[1]!);
  }
  return s.replace(PARTIAL_PLACEHOLDER, (_, path: string) => {
    const v = store.get(path);
    if (typeof v === "string") return v;
    if (v === undefined || v === null) return String(v);
    return JSON.stringify(v);
  });
}

// ═══════════════════════════════════════════════════════════════════
// Compile
// ═══════════════════════════════════════════════════════════════════

// Compiler — turns a signal into a validated Workflow via one LLM call (with
// up to N retries on schema/JSON failure). Always uses the `compiler` preset
// (currently Gemini 3 Flash, reasoning_effort=low): in Test A the Gemini-3
// generation rebuilt the non-obvious dedup step 5/5 with reliable structured
// output, at a fraction of gpt-5.4's cost, and at "low" effort ~2.8s/plan
// (gpt-5.4-class latency, vs ~12s on Gemini's default budget). We emit ONE
// workflow per signal, so reliability + cost + latency all win here. (Override
// the model with AGENT_COMPILER_MODEL; routing follows the name.)

const COMPILER_PRESET: PresetName = "compiler";
// On-disk skill file keeps its historical name `planner.md` (see
// skills.default/) — only the in-code terminology moved to "compiler".
const COMPILER_SKILL_NAME = "planner";

export type CompilerFailureReason =
  | "skill_not_found"
  | "llm_error"
  | "invalid_json"
  | "schema_invalid";

export type CompilerResult =
  | { ok: true; workflow: Workflow; attempts: number }
  | {
      ok: false;
      reason: CompilerFailureReason;
      errors: string[];
      attempts: number;
    };

// Carried context for a replan pass. The previous pass emitted a `replan`
// step; the runtime collected the named bindings and loops back here so
// this pass can plan with data it didn't have before.
export interface PriorContext {
  // 1-based replan pass number (1 = first replan, after the initial plan).
  pass: number;
  // No more replans allowed after this pass — the prompt forces a commit.
  lastPass: boolean;
  // The carried bindings (name → value), also seeded into the store under
  // `context.<name>` so this pass's workflow can reference them.
  data: Record<string, unknown>;
  // Optional note the previous pass left for this one.
  note?: string;
}

export interface CompileRequest {
  abortSignal?: AbortSignal;
  signal: {
    source: string;
    content: string;
    envContext: string | null;
  };
  envData: EnvData;
  parentTrace: TraceContext;
  signalLabel: string;
  // Present only on replan passes. Absent on the initial plan.
  priorContext?: PriorContext;
}

export interface Compiler {
  compile(req: CompileRequest): Promise<CompilerResult>;
}

// Surface that compile.ts depends on. Real Engine matches structurally;
// mocks can be plain objects (mirrors the executor's EngineSurface).
export interface CompilerEngineSurface {
  readonly presets: Record<PresetName, ModelPreset>;
  resolveProvider(model: string): ChatProvider;
}

export interface CompilerDeps {
  engine: CompilerEngineSurface;
  readSkill: (name: string) => Promise<string | null>;
  // Optional improver patch loader for the planner skill. When present and a
  // `skills/planner.patch.md` exists, it's appended to the END of the system
  // message (after the <tools>/<skills> block) — the SAME placement the gate
  // replay measures. Absent → no patch (default; tests omit it).
  readPatch?: (name: string) => Promise<string | null>;
  // Full MCP tool definitions — used to (a) build the schema enum of
  // legal tool names and (b) render compact `name(arg: type, ...)`
  // signatures in the user prompt so the compiler emits the right
  // parameter names. Without this the compiler would guess args from
  // training-data conventions (e.g. `limit` instead of `k` on
  // search_news) and miss filter parameters like `sinceISO`.
  mcpTools: readonly ChatCompletionTool[];
  // All skills that exist on disk. Compiler emits only these.
  knownSkills: readonly string[];
  // Initial attempt + retries. Default 3 (1 attempt + 2 retries).
  maxAttempts?: number;
}

export function createCompiler(deps: CompilerDeps): Compiler {
  const maxAttempts = deps.maxAttempts ?? 3;
  const knownTools = deps.mcpTools.map((t) => t.function.name);
  // WorkflowSchema is rebuilt once per compiler instance — tool/skill enums
  // are baked in. If MCP picks up a new tool at runtime, re-create the
  // compiler (or accept that the new tool can't appear in workflows until
  // restart). The supervisor builds the compiler at engine startup, so
  // this matches process lifecycle.
  const { WorkflowSchema } = createWorkflowSchema({
    knownTools,
    knownSkills: deps.knownSkills,
  });
  // Pre-render tool signatures once — the same prompt content per
  // signal, no point doing this in the hot path.
  const toolSignatures = deps.mcpTools.map(renderToolSignature);
  // The static reference block (tool signatures + skill list) is identical
  // for every signal, so build it once and APPEND IT TO THE SYSTEM MESSAGE
  // (after the planner skill). Keeping all the static content at the front
  // of the request, before any per-signal text, maximises OpenAI's
  // automatic prompt-cache prefix: the model caches the longest common
  // leading token run across calls, so planner.md + tools + skills all land
  // in the cached region. Only the variable signal/env/context stays in the
  // user message. (Caching is purely a prefix optimisation — no API flag.)
  const staticReference = renderStaticReference(deps, toolSignatures);

  return {
    async compile(req) {
      const skill = await deps.readSkill(COMPILER_SKILL_NAME);
      if (skill === null) {
        return {
          ok: false,
          reason: "skill_not_found",
          errors: [`compiler skill "${COMPILER_SKILL_NAME}" not found`],
          attempts: 0,
        };
      }

      const preset = deps.engine.presets[COMPILER_PRESET];
      const provider = deps.engine.resolveProvider(preset.model);

      // System = static prefix (planner rules + tools + skills), cached
      // across signals. User = only the per-signal variable content. A live
      // improver patch (if any) is appended at the very end so the cache prefix
      // stays intact and the placement matches the gate replay.
      const patch = deps.readPatch ? await deps.readPatch(COMPILER_SKILL_NAME) : null;
      const systemContent = appendPatch(`${skill}\n\n${staticReference}`, patch ?? "");
      const initialUserPrompt = renderSignalPrompt(req);
      const messages: ChatCompletionMessageParam[] = [
        { role: "system", content: systemContent },
        { role: "user", content: initialUserPrompt },
      ];

      const compileSpan = req.parentTrace.span({
        // Span name kept as "planner" for trace continuity with
        // pre-rename Langfuse history — do not change to "compile".
        name: "planner",
        kind: "chain",
        input: { preset: COMPILER_PRESET, model: preset.model },
        metadata: { signal_source: req.signal.source },
      });

      try {
        return await runRetryLoop(
          provider,
          preset,
          messages,
          WorkflowSchema,
          compileSpan,
          maxAttempts,
          req.abortSignal,
        );
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        compileSpan.end({ level: "ERROR", statusMessage: message });
        throw err;
      }
    },
  };
}

function parseGeneratedWorkflow(text: string, schema: Parameters<typeof parseWorkflow>[1]): ReturnType<typeof parseWorkflow> {
  let parsed: unknown;
  try {
    parsed = JSON.parse(text);
  } catch (error) {
    return { ok: false, errors: [`invalid JSON: ${toError(error).message}`] };
  }
  return parseWorkflow(parsed, schema);
}

async function runRetryLoop(
  provider: ChatProvider,
  preset: ModelPreset,
  messages: ChatCompletionMessageParam[],
  // Avoid importing the schema type just for the parameter signature.
  // `unknown` here is fine — parseWorkflow accepts any ZodTypeAny.
  schema: Parameters<typeof parseWorkflow>[1],
  compileSpan: Span,
  maxAttempts: number,
  abortSignal?: AbortSignal,
): Promise<CompilerResult> {
  let attempts = 0;
  let lastErrors: string[] = [];

  while (attempts < maxAttempts) {
    attempts++;

    let generated;
    try {
      generated = await Effect.runPromise(traceGenerationEffect({
        scope: compileSpan,
        observation: {
          name: `attempt-${attempts}`, model: preset.model, input: structuredClone(messages),
          modelParameters: { response_format: "json_object" },
        },
        run: async (signal) => {
          const result = await provider.complete({
            model: preset.model, messages, reasoningEffort: preset.reasoningEffort,
            responseFormat: { type: "json_object" }, trace: compileSpan, signal,
          });
          const text = result.message.content ?? "";
          return { text, usage: result.usage, parsed: parseGeneratedWorkflow(text, schema) };
        },
        describe: ({ text, usage, parsed }) => ({
          output: text, usage,
          ...(parsed.ok ? { metadata: { [JUDGE_NODE_META]: "planner" } } : {}),
        }),
      }), { signal: abortSignal });
    } catch (err) {
      abortSignal?.throwIfAborted();
      const message = err instanceof Error ? err.message : String(err);
      compileSpan.end({
        level: "ERROR",
        statusMessage: message,
        output: { reason: "llm_error", attempts },
      });
      return { ok: false, reason: "llm_error", errors: [message], attempts };
    }

    const { text, parsed: result } = generated;
    if (result.ok) {
      compileSpan.end({ output: { attempts, ok: true } });
      return { ok: true, workflow: result.workflow, attempts };
    }

    // Schema-invalid attempt — also UNtagged (not judged), then retry.
    lastErrors = result.errors;
    pushRetryFeedback(messages, text, lastErrors);
  }

  compileSpan.end({
    level: "ERROR",
    statusMessage: "exhausted retries",
    output: { reason: "max_retries", lastErrors, attempts },
  });
  return {
    ok: false,
    reason: lastErrors[0]?.startsWith("invalid JSON")
      ? "invalid_json"
      : "schema_invalid",
    errors: lastErrors,
    attempts,
  };
}

function pushRetryFeedback(
  messages: ChatCompletionMessageParam[],
  lastReply: string,
  errors: string[],
): void {
  messages.push({ role: "assistant", content: lastReply });
  messages.push({
    role: "user",
    content: [
      "Your previous workflow failed validation. Errors:",
      ...errors.map((e) => `  - ${e}`),
      "",
      "Emit a corrected workflow. Return ONLY the JSON, no markdown wrapper.",
    ].join("\n"),
  });
}

// Static half of the prompt — identical for every signal, so it's built
// once and appended to the system message to sit in the cache prefix (see
// createCompiler). Holds the available tools and skills, the reference
// material the compiler needs but that never varies per call.
function renderStaticReference(
  deps: CompilerDeps,
  toolSignatures: string[],
): string {
  const lines: string[] = [];

  lines.push("<tools>");
  lines.push(
    "Signature format: name(arg: type, opt?: type) — description. " +
      "Use the EXACT parameter names listed; do not invent aliases.",
  );
  for (const sig of toolSignatures) {
    lines.push(sig);
  }
  lines.push("</tools>");
  lines.push("");

  lines.push("<skills>");
  for (const name of deps.knownSkills) {
    lines.push(`- ${name}`);
  }
  lines.push("</skills>");

  return lines.join("\n");
}

// Variable half of the prompt — the per-signal content (signal, env,
// envContext, replan context) plus the final emit instruction. Everything
// here changes call-to-call, so it stays in the user message AFTER the
// cached static prefix.
function renderSignalPrompt(req: CompileRequest): string {
  const lines: string[] = [];

  lines.push("<signal>");
  lines.push(`Source: ${req.signal.source}`);
  lines.push(`Content:`);
  lines.push(req.signal.content);
  lines.push("</signal>");
  lines.push("");

  lines.push("<env>");
  lines.push(`Timezone: ${req.envData.timezone}`);
  lines.push(`Now: ${req.envData.now.toISOString()}`);
  if (req.envData.userEmail) {
    lines.push(`User email: ${req.envData.userEmail}`);
  }
  lines.push(
    `News last read at: ${req.envData.newsLastReadAt ?? "never (bootstrap with now - 24h)"}`,
  );
  lines.push("</env>");
  lines.push("");

  if (req.signal.envContext) {
    lines.push("<envContext>");
    lines.push(req.signal.envContext);
    lines.push("</envContext>");
    lines.push("");
  }

  if (req.priorContext) {
    const pc = req.priorContext;
    lines.push("<context>");
    lines.push(
      `You already ran a gather pass for this signal (replan pass ${pc.pass}). ` +
        "Use what you gathered to decide, and emit the ACTING workflow now.",
    );
    lines.push(
      pc.lastPass
        ? "This is your LAST pass — you MUST act now; do NOT emit another `replan`."
        : "Emit another `replan` ONLY if you still genuinely lack data to proceed.",
    );
    if (pc.note) {
      lines.push("");
      lines.push(`Note from your previous pass: ${pc.note}`);
    }
    lines.push("");
    lines.push(
      "Gathered data (also in the store as ${context.<name>} for your steps to reference):",
    );
    lines.push(JSON.stringify(pc.data, null, 2));
    lines.push("</context>");
    lines.push("");
  }

  lines.push(
    "Emit a Workflow as JSON matching the DSL. Return ONLY the JSON, no markdown wrapper.",
  );

  return lines.join("\n");
}

// Compact `name(arg: type, opt?: type) — description` line per tool.
// Keeps the prompt small while giving the compiler enough to use the
// right parameter names — without this it falls back on training-data
// conventions (e.g. `limit` instead of `k`) and silently produces
// invalid args that MCP may or may not accept.
function renderToolSignature(tool: ChatCompletionTool): string {
  const fn = tool.function;
  const params = fn.parameters as
    | {
        properties?: Record<string, unknown>;
        required?: string[];
      }
    | undefined;
  const props = params?.properties ?? {};
  const required = new Set(params?.required ?? []);
  const paramStrs: string[] = [];
  for (const [name, schema] of Object.entries(props)) {
    const type = simplifyJsonSchemaType(schema);
    const opt = required.has(name) ? "" : "?";
    paramStrs.push(`${name}${opt}: ${type}`);
  }
  const sig = `${fn.name}(${paramStrs.join(", ")})`;
  const desc = fn.description ? ` — ${truncate(fn.description, 200)}` : "";
  return `- ${sig}${desc}`;
}

function truncate(s: string, max: number): string {
  if (s.length <= max) return s;
  return s.slice(0, max - 1).trimEnd() + "…";
}

// Best-effort JSON-Schema → short type string. We don't aim for
// completeness — the compiler LLM just needs enough to pick the right
// parameter name and shape. Unknown / weird shapes fall through to
// "any" rather than blocking.
function simplifyJsonSchemaType(schema: unknown): string {
  if (!schema || typeof schema !== "object") return "any";
  const s = schema as {
    type?: string;
    enum?: unknown[];
    items?: unknown;
    description?: string;
  };
  if (Array.isArray(s.enum) && s.enum.length > 0) {
    return s.enum.map((v) => JSON.stringify(v)).join("|");
  }
  switch (s.type) {
    case "string":
      return "string";
    case "number":
    case "integer":
      return "number";
    case "boolean":
      return "boolean";
    case "array":
      return `${simplifyJsonSchemaType(s.items)}[]`;
    case "object":
      return "object";
    default:
      return "any";
  }
}

// ═══════════════════════════════════════════════════════════════════
// Execute
// ═══════════════════════════════════════════════════════════════════

// Base skill prepended to the system message of every `llm_compose` step.
// Defines the contract of a tool-less one-shot call so the model doesn't try
// to "call a tool" and leak its native tool-call markup into the bound text.
const COMPOSER_BASE_SKILL = "composer";

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
//     llm_compose is a fresh API call; llm_agent spawns a fresh AgentLoop)

export type ExecFailureReason =
  | "step_failed"
  | "missing_binding"
  | "duplicate_binding"
  | "skill_not_found"
  | "tool_error"
  | "llm_error";

// A `replan` step asks the runtime to recompile with `context` (the named
// bindings' values) carried forward. The executor surfaces it; the
// workflow facade loops back into the compiler.
export interface ReplanRequest {
  context: Record<string, unknown>;
  note?: string;
}

export type ExecResult =
  | { ok: true; store: VariableStore; replan?: ReplanRequest }
  | {
      ok: false;
      reason: ExecFailureReason;
      error: Error;
      stepIndex: number;
      step: Step;
      store: VariableStore;
    };

export interface ExecContext {
  abortSignal?: AbortSignal;
  sessionContext: SessionContext;
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
  resolveProvider(model: string): ChatProvider;
  mcp: {
    callTool(name: string, args: Record<string, unknown>, options?: { signal?: AbortSignal }): Promise<string>;
  };
  startAgentLoop(opts: AgentLoopOpts): Promise<AgentLoopHandle>;
  endAgentLoop(id: string): void | Promise<void>;
}

// Surface of AgentLoop that the executor touches when running an
// `llm_agent` step. The real AgentLoop has many more methods/fields — we
// only need these two.
export interface AgentLoopHandle {
  messages: ChatCompletionMessageParam[];
  run(): Promise<string>;
}

export interface ExecutorDeps {
  engine: EngineSurface;
  // Decoupled from the skill store so tests can pass a stub. Returns the
  // skill body (no frontmatter); null when not found.
  readSkill: (name: string) => Promise<string | null>;
  // Optional improver patch loader. When present and a `skills/<skill>.patch.md`
  // exists, it's appended to the END of the compose node's system message — the
  // SAME placement the gate replay measures. Absent → no patch (default; tests
  // omit it). Agent (llm_agent) skills are NOT patched yet (judge-only).
  readPatch?: (name: string) => Promise<string | null>;
  // Agent-side memory KV writer (agent.db). A `set_memory` tool step is
  // dispatched here, NOT to MCP — set_memory is a synthetic agent-side
  // tool with no MCP counterpart. Injected (rather than imported) so the
  // executor stays decoupled from db/memory and tests can spy on it.
  setMemory: (key: string, value: string) => void;
  // Sandboxed code-execution backend (Codex) for `code_agent` tool steps.
  // Optional so eval/test executors that never emit a code_agent step need
  // not wire it; a code_agent step without it fails as a tool_error.
  codex?: CodexClient;
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
          ctx.abortSignal?.throwIfAborted();
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
          if (outcome.replan) {
            rootSpan.end({
              output: {
                replanAtIndex: i,
                carried: Object.keys(outcome.replan.context),
              },
            });
            return { ok: true, replan: outcome.replan, store: ctx.store };
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
  | { ok: true; stop: boolean; replan?: ReplanRequest }
  | { ok: false; reason: ExecFailureReason; error: Error };

// A step either continues (stop:false), terminates the pass (stop:true),
// or terminates the pass with a replan request. `output` is the real
// result to record on the step span (the tool's parsed output, the
// composed text, the sub-agent's answer, the replan's carried context).
// runOneStep / execParallel record it on `end`; only when a step has no
// meaningful output do they fall back to a generic control marker. This
// keeps the span's output from being clobbered by a bare `{ ok: true }`.
interface DispatchResult {
  stop: boolean;
  replan?: ReplanRequest;
  output?: unknown;
}

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
    const result = await dispatch(step, store, span, ctx, deps);
    span.end({ output: spanOutput(result) });
    return { ok: true, stop: result.stop, replan: result.replan };
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

// What to record as a step span's output: the step's real result if it
// produced one, otherwise a generic control marker so the span still
// closes with *something* legible (terminal/parallel have no single value).
function spanOutput(result: DispatchResult): unknown {
  if (result.output !== undefined) return result.output;
  if (result.replan) return { replan: true };
  if (result.stop) return { stop: true };
  return { ok: true };
}

// Trace observation kind per step, so each renders with the right badge in
// the UI: a tool step IS a tool call; an llm_agent step spawns a sub-agent;
// llm_compose / parallel are multi-part links in the chain. `code_agent` is a
// `tool` step structurally, but it delegates to a sandboxed code agent (Codex
// writes + runs code), so badge it as an agent like llm_agent / invoke_sub_agent.
// Trace observation kind per step kind, so each renders with the right badge
// in the UI: a tool step IS a tool call; llm_agent / code_agent spawn a
// sub-agent; llm_compose / parallel are multi-part links in the chain.
function stepSpanKind(kind: Step["kind"]): SpanKind {
  switch (kind) {
    case "tool":
      return "tool";
    case "llm_agent":
    case "code_agent":
      return "agent";
    case "llm_compose":
    case "parallel":
    case "replan":
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
    case "code_agent":
      return { bind: step.bind, has_data: step.data !== undefined };
    case "parallel":
      return { child_count: step.steps.length };
    case "terminal":
      return {};
    case "replan":
      return { context: step.context, has_note: step.note !== undefined };
  }
}

function classifyError(err: Error): ExecFailureReason {
  if (err instanceof MissingBindingError) return "missing_binding";
  if (err instanceof DuplicateBindingError) return "duplicate_binding";
  if (err instanceof SkillNotFoundError) return "skill_not_found";
  if (err instanceof ToolCallError) return "tool_error";
  if (err instanceof LlmCallError) return "llm_error";
  return "step_failed";
}

async function dispatch(
  step: Step,
  store: VariableStore,
  span: Span,
  ctx: ExecContext,
  deps: ExecutorDeps,
): Promise<DispatchResult> {
  ctx.abortSignal?.throwIfAborted();
  switch (step.kind) {
    case "tool":
      return { stop: false, output: await execTool(step, store, span, deps, ctx.abortSignal) };
    case "llm_compose":
      return { stop: false, output: await execLlmCompose(step, store, span, deps, ctx.abortSignal) };
    case "llm_agent":
      return { stop: false, output: await execLlmAgent(step, store, span, ctx, deps) };
    case "code_agent":
      return { stop: false, output: await execCodeAgent(step, store, span, deps, ctx.abortSignal) };
    case "parallel":
      await execParallel(step, store, span, ctx, deps);
      return { stop: false };
    case "terminal":
      return { stop: true };
    case "replan": {
      const { request, output } = execReplan(step, store, span);
      return { stop: true, replan: request, output };
    }
  }
}

// ─── replan ──────────────────────────────────────────────────────────

// Collect the named bindings into a context object for the next planning
// pass. A binding the planner names but never bound is dropped (recorded
// in the span) rather than throwing — the next pass simply won't see it,
// same lenient stance as a missing `${}` would surface to the planner.
function execReplan(
  step: ReplanStep,
  store: VariableStore,
  span: Span,
): { request: ReplanRequest; output: { carried: string[]; missing: string[] } } {
  const context: Record<string, unknown> = {};
  const missing: string[] = [];
  for (const name of step.context) {
    if (store.has(name)) context[name] = store.get(name);
    else missing.push(name);
  }
  span.update({ input: { context: step.context, note: step.note ?? null } });
  return {
    request: { context, note: step.note },
    output: { carried: Object.keys(context), missing },
  };
}

// ─── tool ────────────────────────────────────────────────────────────

class ToolCallError extends Error {
  constructor(public readonly tool: string, message: string) {
    super(`tool ${tool} failed: ${message}`);
    this.name = "ToolCallError";
  }
}

// Returns the tool's result (parsed JSON when the payload is an object/array,
// otherwise the raw string) so the caller can record it as the span output.
// set_memory returns its `{ ok, key }` ack. The result is also bound into the
// store under `step.bind` for later `${}` references.
async function execTool(
  step: ToolStep,
  store: VariableStore,
  span: Span,
  deps: ExecutorDeps,
  signal?: AbortSignal,
): Promise<unknown> {
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
    return out;
  }

  let raw: string;
  try {
    raw = await deps.engine.mcp.callTool(step.tool, resolvedArgs, { signal });
  } catch (err) {
    signal?.throwIfAborted();
    throw new ToolCallError(step.tool, toError(err).message);
  }
  signal?.throwIfAborted();

  // MCP error responses come back as text starting with `[tool error]`.
  // Surface those as ToolCallError so the executor classifies correctly.
  if (raw.startsWith("[tool error]")) {
    throw new ToolCallError(step.tool, raw);
  }

  const parsed = tryParseJson(raw);
  if (step.bind) {
    store.set(step.bind, parsed);
  }
  return parsed;
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
// SetMemoryArgsSchema validation as the synthetic-tools registry; on bad
// args we throw ToolCallError so the executor classifies it as a tool
// failure like any other step.
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

// ─── code_agent ──────────────────────────────────────────────────────

// Delegate a computational/coding task to the Codex sandbox (it writes + runs
// code, returns the result). A sub-agent delegation like llm_agent — a Codex
// failure (or a missing backend) propagates and classifies as step_failed,
// routing to the supervisor's fallback path.
async function execCodeAgent(
  step: CodeAgentStep,
  store: VariableStore,
  span: Span,
  deps: ExecutorDeps,
  signal?: AbortSignal,
): Promise<string> {
  if (!deps.codex) throw new Error("code_agent: codex backend not configured");
  const task = substituteText(step.task, store);
  const data = step.data !== undefined ? substituteText(step.data, store) : undefined;
  span.update({ input: { task, has_data: data !== undefined } });

  const result = await runCodeAgent(deps.codex, { task, data }, signal);
  signal?.throwIfAborted();
  store.set(step.bind, result);
  return result;
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
  signal?: AbortSignal,
): Promise<unknown> {
  const preset = deps.engine.presets[step.preset];
  const provider = deps.engine.resolveProvider(preset.model);

  // Base composer skill — loaded for EVERY compose step, ahead of any
  // step-specific skill. It states the rules of a tool-less compose (no tools
  // in this turn; never emit tool-call markup; if you can't answer, say so).
  // This is the prompt-layer guard against a provider leaking native tool-call
  // tokens as text when it wants to act but has none — most compiled compose
  // steps carry no `step.skill` and would otherwise run with no system at all.
  // Absent (null) only in tests / minimal layouts → falls back to prior shape.
  const baseBody = await deps.readSkill(COMPOSER_BASE_SKILL);

  let system: string | undefined = baseBody ?? undefined;
  if (step.skill) {
    const body = await deps.readSkill(step.skill);
    if (body === null) throw new SkillNotFoundError(step.skill);
    // Append the live improver patch (if any) at the end — same placement the
    // gate replay measures, so prod runs exactly what the gate scored.
    const patch = deps.readPatch ? await deps.readPatch(step.skill) : null;
    const stepSystem = appendPatch(body, patch ?? "");
    // Base rules first (stable prefix → prompt-cache friendly), the step's
    // domain skill layered on top.
    system = baseBody ? `${baseBody}\n\n${stepSystem}` : stepSystem;
  }

  const resolvedInput: Record<string, unknown> = {};
  for (const [k, v] of Object.entries(step.input)) {
    resolvedInput[k] = substitute(v, store);
  }
  const userPrompt = step.prompt ? substituteText(step.prompt, store) : "";
  const inputBlocks = renderInputAsXml(resolvedInput);
  const userText = userPrompt
    ? inputBlocks
      ? `${userPrompt}\n\n${inputBlocks}`
      : userPrompt
    : inputBlocks;

  const messages: ChatCompletionMessageParam[] = [];
  if (system) messages.push({ role: "system", content: system });
  messages.push({ role: "user", content: userText });

  let content: string;
  try {
    const result = await runGeneration({
      provider,
      params: { model: preset.model, messages, reasoningEffort: preset.reasoningEffort, signal },
      scope: span,
      observation: {
        name: `llm_compose:${step.bind}`,
        modelParameters: { reasoning_effort: preset.reasoningEffort, tools_mode: "none" },
        metadata: { [JUDGE_NODE_META]: "compose", skill: step.skill ?? null },
      },
      describe: (result) => ({ output: result.message.content ?? "", usage: result.usage }),
    });
    content = result.message.content ?? "";
  } catch (err) {
    signal?.throwIfAborted();
    const message = err instanceof Error ? err.message : String(err);
    throw new LlmCallError(message);
  }

  // Honor the DSL's "structured output" contract: when the compose emits a
  // JSON object/array, bind the PARSED value so later steps can dot into it
  // (`${target.cancelId}`). Plain prose / markdown digests don't start with
  // `{`/`[`, so they stay strings — same lenient rule as execTool's tool
  // results. Without this, a compose that returns JSON binds a raw string and
  // any `${bind.field}` reference fails with MissingBindingError.
  signal?.throwIfAborted();
  const parsed = tryParseJson(content);
  store.set(step.bind, parsed);
  return parsed;
}

// Substitution for fields that must end up as TEXT (llm_compose / llm_agent
// prompts). `substitute` preserves the bound value's type when the string is
// one whole placeholder (`"${posts}"` → the actual array) — right for tool
// args, wrong for a prompt: a non-string would end up as a raw object in
// `message.content` (API error) or get default-coerced by a template
// literal. Stringify it explicitly instead.
function substituteText(value: string, store: VariableStore): string {
  const resolved = substitute(value, store);
  return typeof resolved === "string" ? resolved : JSON.stringify(resolved, null, 2);
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
): Promise<string> {
  const prompt = substituteText(step.prompt, store);
  const allowedTools = new Set(step.tools);
  const childId = `${ctx.signalLabel}__agent:${step.bind}`;

  // Record the resolved prompt as the agent step's input so the per-node judge
  // can score this AGENT span black-box (input→output) without descending into
  // the sub-agent's own iterations.
  span.update({ input: { skill: step.skill, prompt } });

  const child = await deps.engine.startAgentLoop({
    id: childId,
    signal: ctx.abortSignal,
    sessionContext: ctx.sessionContext,
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
    await deps.engine.endAgentLoop(childId);
  }

  ctx.abortSignal?.throwIfAborted();
  store.set(step.bind, result);
  return result;
}

// ─── parallel ────────────────────────────────────────────────────────

async function execParallel(
  step: ParallelStep,
  store: VariableStore,
  span: Span,
  ctx: ExecContext,
  deps: ExecutorDeps,
): Promise<void> {
  await Effect.runPromise(Effect.forEach(
    step.steps, (child, i) => Effect.suspend(() => {
      const childSpan = span.span({
        name: `parallel[${i}]:${child.kind}`,
        kind: stepSpanKind(child.kind),
        metadata: stepMetadata(child),
      });
      let pending: Promise<DispatchResult> | undefined;
      return Effect.tryPromise({
        try: (signal) => (pending = dispatch(child, store, childSpan, { ...ctx, abortSignal: signal }, deps)),
        catch: toError,
      }).pipe(Effect.onExit((exit) => Effect.gen(function* () {
        const completion = pending;
        if (child.kind === "llm_agent" && completion) {
          yield* Effect.promise(() => completion.then(() => undefined, () => undefined));
        }
        if (Exit.isSuccess(exit)) childSpan.end({ output: spanOutput(exit.value) });
        else childSpan.end({ level: "ERROR", statusMessage: toError(Cause.squash(exit.cause)).message });
      })));
    }), { concurrency: 4 },
  ), { signal: ctx.abortSignal });
}

// ─── shared helpers exported for the executor's own tests ────────────

export const __testing = {
  renderInputAsXml,
  classifyError,
  ToolCallError,
  SkillNotFoundError,
  LlmCallError,
};

// ═══════════════════════════════════════════════════════════════════
// Runner
// ═══════════════════════════════════════════════════════════════════

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
    sessionContext: SessionContext,
    parentTrace: TraceContext,
    abortSignal?: AbortSignal,
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
  // Optional improver patch loader (skills/<name>.patch.md), threaded to both
  // the compiler (planner patch) and the executor (compose patch). Absent → no
  // patching; agent (llm_agent) skills are judge-only and never patched here.
  readPatch?: (name: string) => Promise<string | null>;
  // Agent-side memory KV writer — dispatched by the executor for
  // `set_memory` tool steps (watermark writes). Injected by the
  // composition root, same instance the AgentLoop path uses.
  setMemory: (key: string, value: string) => void;
  // Sandboxed code-execution backend (Codex) for `code_agent` tool steps.
  // Same instance the AgentLoop path uses; optional for eval/test runners.
  codex?: CodexClient;
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
    readPatch: deps.readPatch,
    // set_memory is a synthetic agent-side tool with no MCP counterpart;
    // surface it to the compiler too so it appears in the schema enum and
    // the prompt's tool signatures (the executor dispatches it directly).
    // (code_agent is a first-class step kind, not a tool — see dsl.ts.)
    mcpTools: [...deps.mcpTools, SET_MEMORY_TOOL],
    knownSkills: deps.knownSkills,
    maxAttempts: deps.maxAttempts,
  });
  const executor = createExecutor({
    engine: deps.engine,
    readSkill: deps.readSkill,
    readPatch: deps.readPatch,
    setMemory: deps.setMemory,
    codex: deps.codex,
  });

  const maxPasses = deps.maxPasses ?? 3;

  return {
    async runForSignal(signal, sessionContext, parentTrace, abortSignal) {
      const envData = sessionContext.env;
      const signalLabel = `${signal.source}:${signal.id}`;
      // The plan→act→replan loop. Pass 0 is the initial plan; each `replan`
      // step carries context into the next pass. Bounded by `maxPasses` so
      // a planner that never commits can't loop forever.
      let priorContext: PriorContext | undefined;
      let lastAttempts = 0;

      for (let pass = 0; pass < maxPasses; pass++) {
        abortSignal?.throwIfAborted();
        const compiled = await compiler.compile({
          abortSignal: abortSignal,
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
          abortSignal,
          sessionContext,
          store,
          parentTrace,
          signalLabel,
        });

        abortSignal?.throwIfAborted();
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
