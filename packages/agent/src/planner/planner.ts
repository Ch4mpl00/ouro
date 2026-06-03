import type OpenAI from "openai";
import type { ChatCompletionMessageParam } from "openai/resources/chat/completions";
import type { ModelPreset, PresetName } from "../models";
import type { EnvData } from "../session-context";
import type { Span, TraceContext } from "../tracing";
import { createPlanSchema, parsePlan, type Plan } from "./dsl";

// Planner — turns a signal into a validated Plan via one LLM call (with
// up to N retries on schema/JSON failure). Always uses the `smartest`
// preset (currently OpenAI gpt-5.4): strict structured-output guarantees
// matter more here than per-call cost — we emit ONE plan per signal and
// the runtime takes over.

const PLANNER_PRESET: PresetName = "smartest";
const PLANNER_SKILL_NAME = "planner";

export type PlannerFailureReason =
  | "skill_not_found"
  | "llm_error"
  | "invalid_json"
  | "schema_invalid";

export type PlannerResult =
  | { ok: true; plan: Plan; attempts: number }
  | {
      ok: false;
      reason: PlannerFailureReason;
      errors: string[];
      attempts: number;
    };

export interface PlanRequest {
  signal: {
    source: string;
    content: string;
    envContext: string | null;
  };
  envData: EnvData;
  parentTrace: TraceContext;
  signalLabel: string;
}

export interface Planner {
  plan(req: PlanRequest): Promise<PlannerResult>;
}

// Surface that planner.ts depends on. Real Engine matches structurally;
// mocks can be plain objects (mirrors the runner's EngineSurface).
export interface PlannerEngineSurface {
  readonly presets: Record<PresetName, ModelPreset>;
  resolveProvider(model: string): {
    client: OpenAI;
    kind: "deepseek" | "openai";
  };
}

export interface PlannerDeps {
  engine: PlannerEngineSurface;
  readSkill: (name: string) => Promise<string | null>;
  // Tools that MCP exposes right now. Planner emits only these.
  knownTools: readonly string[];
  // All skills that exist on disk. Planner emits only these.
  knownSkills: readonly string[];
  // Optional 1-line description per tool — injected into the user
  // prompt so the planner knows what each tool does. Missing entries
  // render as empty; planner falls back on tool naming.
  toolDescriptions?: Record<string, string>;
  // Initial attempt + retries. Default 3 (1 attempt + 2 retries).
  maxAttempts?: number;
}

export function createPlanner(deps: PlannerDeps): Planner {
  const maxAttempts = deps.maxAttempts ?? 3;
  // PlanSchema is rebuilt once per planner instance — tool/skill enums
  // are baked in. If MCP picks up a new tool at runtime, re-create the
  // planner (or accept that the new tool can't appear in plans until
  // restart). The supervisor builds the planner at engine startup, so
  // this matches process lifecycle.
  const { PlanSchema } = createPlanSchema({
    knownTools: deps.knownTools,
    knownSkills: deps.knownSkills,
  });

  return {
    async plan(req) {
      const skill = await deps.readSkill(PLANNER_SKILL_NAME);
      if (skill === null) {
        return {
          ok: false,
          reason: "skill_not_found",
          errors: [`planner skill "${PLANNER_SKILL_NAME}" not found`],
          attempts: 0,
        };
      }

      const preset = deps.engine.presets[PLANNER_PRESET];
      const provider = deps.engine.resolveProvider(preset.model);

      const initialUserPrompt = renderUserPrompt(req, deps);
      const messages: ChatCompletionMessageParam[] = [
        { role: "system", content: skill },
        { role: "user", content: initialUserPrompt },
      ];

      const plannerSpan = req.parentTrace.span({
        name: "planner",
        input: { preset: PLANNER_PRESET, model: preset.model },
        metadata: { signal_source: req.signal.source },
      });

      try {
        return await runRetryLoop(
          provider.client,
          preset,
          messages,
          PlanSchema,
          plannerSpan,
          maxAttempts,
        );
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        plannerSpan.end({ level: "ERROR", statusMessage: message });
        throw err;
      }
    },
  };
}

async function runRetryLoop(
  client: OpenAI,
  preset: ModelPreset,
  messages: ChatCompletionMessageParam[],
  // Avoid importing the schema type just for the parameter signature.
  // `unknown` here is fine — parsePlan accepts any ZodTypeAny.
  schema: Parameters<typeof parsePlan>[1],
  plannerSpan: Span,
  maxAttempts: number,
): Promise<PlannerResult> {
  let attempts = 0;
  let lastErrors: string[] = [];

  while (attempts < maxAttempts) {
    attempts++;

    const gen = plannerSpan.generation({
      name: `attempt-${attempts}`,
      model: preset.model,
      input: messages,
      modelParameters: { response_format: "json_object" },
    });

    let text: string;
    try {
      const response = await client.chat.completions.create({
        model: preset.model,
        messages,
        response_format: { type: "json_object" },
      });
      text = response.choices[0]?.message.content ?? "";
      const u = response.usage;
      gen.end({
        output: text,
        usage: u
          ? {
              input: u.prompt_tokens,
              output: u.completion_tokens,
              total: u.total_tokens,
            }
          : undefined,
      });
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      gen.end({
        output: { error: message },
        level: "ERROR",
        statusMessage: message,
      });
      plannerSpan.end({
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

    const result = parsePlan(parsed, schema);
    if (result.ok) {
      plannerSpan.end({ output: { attempts, ok: true } });
      return { ok: true, plan: result.plan, attempts };
    }

    lastErrors = result.errors;
    pushRetryFeedback(messages, text, lastErrors);
  }

  plannerSpan.end({
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
      "Your previous plan failed validation. Errors:",
      ...errors.map((e) => `  - ${e}`),
      "",
      "Emit a corrected plan. Return ONLY the JSON, no markdown wrapper.",
    ].join("\n"),
  });
}

function renderUserPrompt(req: PlanRequest, deps: PlannerDeps): string {
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

  lines.push("<tools>");
  for (const name of deps.knownTools) {
    const desc = deps.toolDescriptions?.[name];
    lines.push(desc ? `- ${name}: ${desc}` : `- ${name}`);
  }
  lines.push("</tools>");
  lines.push("");

  lines.push("<skills>");
  for (const name of deps.knownSkills) {
    lines.push(`- ${name}`);
  }
  lines.push("</skills>");
  lines.push("");

  lines.push(
    "Emit a Plan as JSON matching the DSL. Return ONLY the JSON, no markdown wrapper.",
  );

  return lines.join("\n");
}
