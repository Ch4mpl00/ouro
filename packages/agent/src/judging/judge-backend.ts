import OpenAI from "openai";
import { createCodexClient, type CodexClient } from "./codex-client";
import { JUDGE_MODEL } from "./schema";

// A judge backend runs ONE structured-output completion: a system instruction
// + user content + a strict JSON schema → the raw parsed JSON object, which the
// caller validates with Zod. This is the SINGLE primitive that differs between
// judge providers (OpenAI chat vs the codex service). Everything provider-
// agnostic — rubric selection, prompt building, the scorecard+faithfulness
// split — lives in node-judge.ts and depends only on this interface.
export interface JudgeCompletion {
  // Schema name (OpenAI's json_schema label, e.g. "scorecard"); ignored by
  // backends that don't take one.
  name: string;
  system: string;
  user: string;
  schema: Record<string, unknown>;
}

export interface JudgeBackend {
  complete(req: JudgeCompletion): Promise<unknown>;
}

export function createOpenAiJudgeBackend(openai: OpenAI): JudgeBackend {
  return {
    async complete({ name, system, user, schema }) {
      const res = await openai.chat.completions.create({
        model: JUDGE_MODEL,
        messages: [
          { role: "system", content: system },
          { role: "user", content: user },
        ],
        response_format: { type: "json_schema", json_schema: { name, strict: true, schema } },
      });
      const content = res.choices[0]?.message.content;
      if (!content) throw new Error("judge returned empty content");
      return JSON.parse(content);
    },
  };
}

export function createCodexJudgeBackend(codex: CodexClient): JudgeBackend {
  const timeoutMs = Number(process.env.CODEX_JUDGE_TIMEOUT_MS ?? 10 * 60_000);
  return {
    async complete({ system, user, schema }) {
      const result = await codex.run({
        prompt: `${system}\n\nReturn only the final JSON object matching the provided schema.`,
        input: user,
        schema,
        sandbox: "read-only",
        approvalPolicy: "never",
        timeoutMs,
        config: {
          web_search: "disabled",
          "features.shell_tool": false,
          "features.multi_agent": false,
        },
      });
      // The codex service may pre-parse to `parsed`; fall back to the raw text.
      return result.parsed !== undefined ? result.parsed : JSON.parse(result.content);
    },
  };
}

export type JudgeProvider = "openai" | "codex";

function openAiFromEnv(): OpenAI {
  const apiKey = process.env.OPENAI_API_KEY;
  if (!apiKey) throw new Error("OPENAI_API_KEY missing in env");
  return new OpenAI({ apiKey });
}

// Resolve a provider name to its backend. `openai` may be passed by a caller
// that already built a client (the CLI); otherwise it's created from env.
export function createJudgeBackend(provider: JudgeProvider, openai?: OpenAI | null): JudgeBackend {
  if (provider === "codex") return createCodexJudgeBackend(createCodexClient());
  return createOpenAiJudgeBackend(openai ?? openAiFromEnv());
}
