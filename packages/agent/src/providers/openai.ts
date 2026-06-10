import type OpenAI from "openai";
import type { ChatCompletionCreateParamsNonStreaming } from "openai/resources/chat/completions";
import type { ChatProvider, CompletionParams, CompletionResult } from "./types";
import { normalizeOpenAiUsage } from "./usage";
import { toResult } from "./result";

// OpenAI provider. `reasoningEffort` is INTENTIONALLY not forwarded: for the
// OpenAI models we route here (gpt-5.4 / gpt-5.4-mini — including when the
// workflow compiler runs on one via AGENT_COMPILER_MODEL) the default,
// effort-less request measured as the best speed/quality/price point, so a
// preset's `reasoningEffort` is a no-op on this route (it only drives the
// DeepSeek/Gemini providers). tools / response_format pass through unchanged.
export function createOpenAiProvider(client: OpenAI): ChatProvider {
  return {
    kind: "openai",
    async complete(params: CompletionParams): Promise<CompletionResult> {
      const hasTools = params.tools && params.tools.length > 0;
      const body: ChatCompletionCreateParamsNonStreaming = {
        model: params.model,
        messages: params.messages,
        ...(hasTools ? { tools: params.tools } : {}),
        ...(params.responseFormat ? { response_format: params.responseFormat } : {}),
      };
      const raw = await client.chat.completions.create(body);
      return toResult(raw, normalizeOpenAiUsage(raw.usage));
    },
  };
}
