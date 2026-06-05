import type OpenAI from "openai";
import type {
  ChatCompletionMessageParam,
  ChatCompletionCreateParamsNonStreaming,
} from "openai/resources/chat/completions";
import type { ChatProvider, CompletionParams, CompletionResult } from "./types";
import { normalizeDeepseekUsage } from "./usage";
import { toResult } from "./result";

// DeepSeek extends OpenAI's assistant message shape with `reasoning_content`
// (the thinking text). It's required in the request history whenever the
// next call uses thinking-mode — even if empty.
type DeepSeekAssistantHistory = ChatCompletionMessageParam & {
  reasoning_content?: string;
};

// Stamp an empty `reasoning_content` on every assistant turn missing it.
// Turns produced under thinking-disabled (or by OpenAI) lack the field, and
// a thinking-enabled DeepSeek call 400s if any prior assistant turn is
// missing it. Mutates in place — the caller's history array IS the
// conversation that must stay valid for the next call.
function ensureReasoningContentOnHistory(messages: ChatCompletionMessageParam[]): void {
  for (const m of messages) {
    if (m.role !== "assistant") continue;
    const extended = m as DeepSeekAssistantHistory;
    if (extended.reasoning_content === undefined) {
      extended.reasoning_content = "";
    }
  }
}

// DeepSeek provider. Adds `thinking` + `reasoning_effort` on top of the
// OpenAI request shape, and repairs the assistant history before a
// thinking-enabled send.
export function createDeepseekProvider(client: OpenAI): ChatProvider {
  return {
    kind: "deepseek",
    async complete(params: CompletionParams): Promise<CompletionResult> {
      const thinkingEnabled = params.reasoningEffort !== "disabled";
      if (thinkingEnabled) ensureReasoningContentOnHistory(params.messages);

      const hasTools = params.tools && params.tools.length > 0;
      // The OpenAI-valid part stays statically typed as the request contract.
      const body: ChatCompletionCreateParamsNonStreaming = {
        model: params.model,
        messages: params.messages,
        ...(hasTools ? { tools: params.tools } : {}),
        ...(params.responseFormat ? { response_format: params.responseFormat } : {}),
      };
      // DeepSeek extensions the OpenAI types don't model: a `thinking` flag
      // and a `reasoning_effort` value ("max") outside OpenAI's enum. Attach
      // them at runtime via Object.assign — `body` keeps its typed contract
      // (what create() checks against) while still carrying the extra wire
      // fields. No cast, no excess-property fight.
      const extensions: Record<string, unknown> = thinkingEnabled
        ? { thinking: { type: "enabled" }, reasoning_effort: params.reasoningEffort }
        : { thinking: { type: "disabled" } };
      Object.assign(body, extensions);

      const raw = await client.chat.completions.create(body);
      return toResult(raw, normalizeDeepseekUsage(raw.usage));
    },
  };
}
