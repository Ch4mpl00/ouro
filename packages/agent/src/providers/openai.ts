import type OpenAI from "openai";
import type { ChatCompletionCreateParamsNonStreaming } from "openai/resources/chat/completions";
import type { ChatProvider, CompletionParams, CompletionResult } from "./types";
import { normalizeOpenAiUsage } from "./usage";
import { toResult } from "./result";

// OpenAI provider. The models we route here (gpt-5.4 for the compiler,
// gpt-5.4-mini for cheap chat) run WITHOUT an explicit reasoning_effort in
// the request — so we drop it, preserving the pre-abstraction behavior.
// tools / response_format pass through unchanged.
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
