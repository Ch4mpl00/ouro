import type OpenAI from "openai";
import type { ChatCompletionCreateParamsNonStreaming } from "openai/resources/chat/completions";
import type { ChatProvider, CompletionParams, CompletionResult } from "./types";
import { normalizeOpenAiUsage } from "./usage";
import { toResult } from "./result";

// Gemini provider. Google exposes an OpenAI-compatible Chat Completions
// endpoint, so the request/response shape — tools, response_format, usage —
// matches OpenAI's (normalizeOpenAiUsage reads prompt_tokens_details just the
// same). The one divergence we model is reasoning_effort. Gemini's enum is
// none|low|medium|high (no "max"). A latency sweep on Gemini-3 (Test A trace):
//   omit / medium / high → ~12s per plan (dynamic budget is heavy)
//   "low"                → ~2.8s, dedup step still 5/5  ← the compiler uses this
//   "none"               → ~2.7s but drops the dedup step (2/3) — too lossy
// So we map: "disabled" → omit, "low" → "low" (the latency knob), else → "high".
//
// The client is constructed with baseURL GEMINI_BASE_URL in the engine.
//
// Transient-failure retries used to live here (Gemini preview availability
// wobbles); they moved to the provider-agnostic `withRetry` decorator (see
// ./retry.ts) that the engine wraps around every provider — so the compiler
// keeps its hot-path protection no matter which provider its model routes to.
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
      if (params.reasoningEffort === "low") {
        Object.assign(body, { reasoning_effort: "low" });
      } else if (params.reasoningEffort !== "disabled") {
        Object.assign(body, { reasoning_effort: "high" });
      }
      const raw = await client.chat.completions.create(body);
      return toResult(raw, normalizeOpenAiUsage(raw.usage));
    },
  };
}
