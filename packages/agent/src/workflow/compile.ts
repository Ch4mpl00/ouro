import type {
  ChatCompletionMessageParam,
  ChatCompletionTool,
} from "openai/resources/chat/completions";
import type { ModelPreset, PresetName } from "../models";
import type { ChatProvider } from "../providers";
import type { EnvData } from "../session-context";
import type { Span, TraceContext } from "../tracing";
import { createWorkflowSchema, parseWorkflow, type Workflow } from "./dsl";

// Compiler — turns a signal into a validated Workflow via one LLM call (with
// up to N retries on schema/JSON failure). Always uses the `compiler` preset
// (currently Gemini 3.5 Flash): in Test A it rebuilt the non-obvious dedup
// step 10/10 with reliable structured output, at a fraction of gpt-5.4's
// cost — and we emit ONE workflow per signal, so reliability + cost win here.
// (Override the model with AGENT_COMPILER_MODEL; routing follows the name.)

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
      // across signals. User = only the per-signal variable content.
      const systemContent = `${skill}\n\n${staticReference}`;
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
        );
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        compileSpan.end({ level: "ERROR", statusMessage: message });
        throw err;
      }
    },
  };
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
): Promise<CompilerResult> {
  let attempts = 0;
  let lastErrors: string[] = [];

  while (attempts < maxAttempts) {
    attempts++;

    const gen = compileSpan.generation({
      name: `attempt-${attempts}`,
      model: preset.model,
      input: messages,
      modelParameters: { response_format: "json_object" },
    });

    let text: string;
    try {
      // The provider normalizes usage (incl. the cached-prompt portion that
      // shows the static planner.md + tools + skills prefix hitting cache).
      const result = await provider.complete({
        model: preset.model,
        messages,
        reasoningEffort: preset.reasoningEffort,
        responseFormat: { type: "json_object" },
      });
      text = result.message.content ?? "";
      gen.end({ output: text, usage: result.usage });
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      gen.end({
        output: { error: message },
        level: "ERROR",
        statusMessage: message,
      });
      compileSpan.end({
        level: "ERROR",
        statusMessage: message,
        output: { reason: "llm_error", attempts },
      });
      return { ok: false, reason: "llm_error", errors: [message], attempts };
    }

    let parsed: unknown;
    try {
      parsed = JSON.parse(text);
    } catch (err) {
      const errMsg = `invalid JSON: ${(err as Error).message}`;
      lastErrors = [errMsg];
      pushRetryFeedback(messages, text, lastErrors);
      continue;
    }

    const result = parseWorkflow(parsed, schema);
    if (result.ok) {
      compileSpan.end({ output: { attempts, ok: true } });
      return { ok: true, workflow: result.workflow, attempts };
    }

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
