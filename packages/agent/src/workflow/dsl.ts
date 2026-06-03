import { z } from "zod";
import { zodToJsonSchema } from "zod-to-json-schema";
import { PRESET_NAMES } from "../models";

// Workflow DSL — the language the compiler LLM emits and the executor runs.
//
// Five step kinds, no control flow primitives beyond `parallel`:
//
//   tool         — runtime → MCP tool with literal args, result bound
//   llm_compose  — LLM with no tools, structured output, result bound
//   llm_agent    — LLM with a bounded tool whitelist + maxIterations,
//                  internal ReAct loop, final text bound
//   parallel     — flat list of independent leaf steps, run concurrently
//   terminal     — explicit end of workflow
//
// No `branch` / `if` / loops: empty-case handling lives inside
// llm_compose prompts; truly structural decisions ("did this happen?")
// fall back to the agentic mode at the supervisor level.
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

  const TerminalStepSchema = z
    .object({
      kind: z.literal("terminal"),
    })
    .strict();

  // Leaf steps: anything except `parallel`. Used inside `parallel.steps`
  // to enforce flatness — nested `parallel` is intentionally forbidden,
  // both to keep traces readable and to keep the compiler from
  // over-engineering workflows.
  const LeafStepSchema = z.discriminatedUnion("kind", [
    ToolStepSchema,
    LlmComposeStepSchema,
    LlmAgentStepSchema,
    TerminalStepSchema,
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
    TerminalStepSchema,
    ParallelStepSchema,
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

export type PresetName = (typeof PRESET_NAMES)[number];

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

export interface ParallelStep {
  kind: "parallel";
  steps: LeafStep[];
}

export interface TerminalStep {
  kind: "terminal";
}

export type LeafStep = ToolStep | LlmComposeStep | LlmAgentStep | TerminalStep;
export type Step = LeafStep | ParallelStep;

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
