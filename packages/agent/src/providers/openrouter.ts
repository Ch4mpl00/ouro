import OpenAI from "openai";
import type { ChatCompletionCreateParamsNonStreaming } from "openai/resources/chat/completions";
import type { ChatProvider, CompletionParams, CompletionResult } from "./types";
import { normalizeOpenAiUsage } from "./usage";
import { toResult } from "./result";

// OpenRouter provider — the opt-in experimentation route (see
// .claude/tasks/openrouter-provider.md). One OpenAI-compatible endpoint and one
// key in front of hundreds of models, so A/B-ing a vendor we have no direct
// provider for is an env change: AGENT_SMART_MODEL="openrouter/anthropic/…".
//
// Routing is by an explicit `openrouter/` prefix, never by vendor name: a bare
// "deepseek-*" or "gemini-*" keeps going to the direct provider (prompt cache
// and the DeepSeek template are load-bearing there), and only a model string
// that asks for OpenRouter gets it. The prefix is ours — it is stripped before
// the request, OpenRouter itself knows the model as "anthropic/…".
//
// Reasoning goes through OpenRouter's unified `reasoning: { effort }` field,
// which it translates per upstream (effort enum for OpenAI-likes, a token
// budget for Anthropic/Gemini). "disabled" omits it — the model's default, the
// same choice the Gemini provider makes. Its enum tops out at "high", so "max"
// maps there.
//
// Usage is OpenAI-shaped (prompt_tokens_details.cached_tokens included), so
// normalizeOpenAiUsage applies unchanged.

export const OPENROUTER_BASE_URL = "https://openrouter.ai/api/v1";
export const OPENROUTER_MODEL_PREFIX = "openrouter/";

// One construction for every caller (supervisor, bench, decisions). The title
// header labels our traffic on the OpenRouter dashboard.
export function createOpenRouterClient(apiKey: string): OpenAI {
  return new OpenAI({
    apiKey,
    baseURL: OPENROUTER_BASE_URL,
    defaultHeaders: { "X-Title": "mcp-tools" },
  });
}

export function isOpenRouterModel(model: string): boolean {
  return model.startsWith(OPENROUTER_MODEL_PREFIX);
}

export function createOpenRouterProvider(client: OpenAI): ChatProvider {
  return {
    kind: "openrouter",
    async complete(params: CompletionParams): Promise<CompletionResult> {
      const hasTools = params.tools && params.tools.length > 0;
      const body: ChatCompletionCreateParamsNonStreaming = {
        model: params.model.slice(
          isOpenRouterModel(params.model) ? OPENROUTER_MODEL_PREFIX.length : 0,
        ),
        messages: params.messages,
        ...(hasTools ? { tools: params.tools } : {}),
        ...(params.responseFormat ? { response_format: params.responseFormat } : {}),
      };
      // `reasoning` is an OpenRouter extension the OpenAI types don't model —
      // attached at runtime, the same pattern as the deepseek/gemini providers.
      if (params.reasoningEffort !== "disabled") {
        const effort = params.reasoningEffort === "max" ? "high" : params.reasoningEffort;
        Object.assign(body, { reasoning: { effort } });
      }
      const raw = await client.chat.completions.create(body, { signal: params.signal, maxRetries: 0 });
      return toResult(raw, normalizeOpenAiUsage(raw.usage));
    },
  };
}
