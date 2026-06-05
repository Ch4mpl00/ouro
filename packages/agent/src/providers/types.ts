import type {
  ChatCompletionMessage,
  ChatCompletionMessageParam,
  ChatCompletionTool,
  ChatCompletionCreateParamsNonStreaming,
} from "openai/resources/chat/completions";
import type { ReasoningEffort } from "../models";
import type { TokenUsage } from "../tracing";

// Provider abstraction. Both DeepSeek and OpenAI speak the OpenAI Chat
// Completions wire format, but they diverge on two things we kept
// re-branching on at every call site:
//
//   1. Request shape — DeepSeek needs `thinking` + `reasoning_effort` and a
//      `reasoning_content` stamp on prior assistant turns; OpenAI takes
//      neither.
//   2. Usage reporting — the cached-prompt portion lives under
//      `prompt_tokens_details.cached_tokens` (OpenAI) vs
//      `prompt_cache_hit_tokens` (DeepSeek).
//
// `ChatProvider.complete` hides both behind one normalized call/return, so
// session.ts / compile.ts / execute.ts stop carrying `if (kind ===
// "deepseek")` branches. New provider → new factory, no call-site edits.

export type ProviderKind = "deepseek" | "openai";

export interface CompletionParams {
  model: string;
  messages: ChatCompletionMessageParam[];
  // Provider decides how to express it (DeepSeek: thinking + reasoning_effort;
  // OpenAI: dropped — the models we route there run without it).
  reasoningEffort: ReasoningEffort;
  // Omitted / empty → no tools sent (the SDK rejects an empty array).
  tools?: ChatCompletionTool[];
  responseFormat?: ChatCompletionCreateParamsNonStreaming["response_format"];
}

export interface CompletionResult {
  message: ChatCompletionMessage;
  finishReason: string | null;
  // Normalized usage with the cached-input portion filled in per provider.
  usage?: TokenUsage;
}

export interface ChatProvider {
  readonly kind: ProviderKind;
  complete(params: CompletionParams): Promise<CompletionResult>;
}
