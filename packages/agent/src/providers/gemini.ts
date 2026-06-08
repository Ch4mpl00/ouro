import type OpenAI from "openai";
import type { ChatCompletionCreateParamsNonStreaming } from "openai/resources/chat/completions";
import type { ChatProvider, CompletionParams, CompletionResult } from "./types";
import { normalizeOpenAiUsage } from "./usage";
import { toResult } from "./result";

// Gemini provider. Google exposes an OpenAI-compatible Chat Completions
// endpoint, so the request/response shape — tools, response_format, usage —
// matches OpenAI's (normalizeOpenAiUsage reads prompt_tokens_details just the
// same). The one divergence we model is reasoning_effort: Gemini's enum is
// low|medium|high (no "max"), and OMITTING it lets Gemini pick a dynamic
// thinking budget — the exact mode Test A validated, where gemini-3.5-flash
// rebuilt the compiler's dedup step 5/5 with no explicit effort. So we map
// "disabled" → omit (dynamic budget), anything else → "high".
//
// The client is constructed with baseURL
// https://generativelanguage.googleapis.com/v1beta/openai/ in the engine.
export function createGeminiProvider(client: OpenAI): ChatProvider {
  return {
    kind: "gemini",
    async complete(params: CompletionParams): Promise<CompletionResult> {
      const hasTools = params.tools && params.tools.length > 0;
      const body: ChatCompletionCreateParamsNonStreaming = {
        model: params.model,
        messages: params.messages,
        ...(hasTools ? { tools: params.tools } : {}),
        ...(params.responseFormat ? { response_format: params.responseFormat } : {}),
      };
      // reasoning_effort is outside the statically-typed body — attach at
      // runtime, mirroring the deepseek provider's extension pattern.
      if (params.reasoningEffort !== "disabled") {
        Object.assign(body, { reasoning_effort: "high" });
      }
      const raw = await client.chat.completions.create(body);
      return toResult(raw, normalizeOpenAiUsage(raw.usage));
    },
  };
}
