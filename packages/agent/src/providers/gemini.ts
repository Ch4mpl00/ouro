import OpenAI from "openai";
import type { ChatCompletionCreateParamsNonStreaming } from "openai/resources/chat/completions";
import type { ChatProvider, CompletionParams, CompletionResult } from "./types";
import { normalizeOpenAiUsage } from "./usage";
import { toResult } from "./result";

// Gemini provider. Google exposes an OpenAI-compatible Chat Completions
// endpoint, so the request/response shape — tools, response_format, usage —
// matches OpenAI's (normalizeOpenAiUsage reads prompt_tokens_details just the
// same). The one divergence we model is reasoning_effort: Gemini's enum is
// low|medium|high (no "max"), and OMITTING it lets Gemini pick a dynamic
// thinking budget — the exact mode Test A validated, where the Gemini-3
// generation rebuilt the compiler's dedup step 5/5 with no explicit effort. So
// we map "disabled" → omit (dynamic budget), anything else → "high".
//
// The client is constructed with baseURL
// https://generativelanguage.googleapis.com/v1beta/openai/ in the engine.
//
// Unlike the OpenAI/DeepSeek providers, this one RETRIES 429/5xx with backoff:
// the compiler runs on a Gemini preview model whose availability wobbles
// (transient 429 rate-limit / 503 overload), and the compiler is on the hot
// path — a blip should retry, not drop the signal to the agentic fallback.
// 4xx other than 429 are permanent and rethrown immediately.
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
      for (let attempt = 0; ; attempt++) {
        try {
          const raw = await client.chat.completions.create(body);
          return toResult(raw, normalizeOpenAiUsage(raw.usage));
        } catch (err) {
          const status = err instanceof OpenAI.APIError ? err.status : undefined;
          const retryable = status === 429 || (status !== undefined && status >= 500);
          if (!retryable || attempt >= 4) throw err;
          await new Promise((r) => setTimeout(r, 2000 * 2 ** attempt)); // 2s,4s,8s,16s
        }
      }
    },
  };
}
