import type { ChatCompletion } from "openai/resources/chat/completions";
import type { TokenUsage } from "../tracing";
import type { CompletionResult } from "./types";

// Shared mapping from a raw ChatCompletion to our normalized result. Usage
// is normalized per provider (passed in); the message/finishReason mapping
// is identical across providers since both speak the OpenAI wire format.
export function toResult(
  raw: ChatCompletion,
  usage: TokenUsage | undefined,
): CompletionResult {
  const choice = raw.choices[0]!;
  return {
    message: choice.message,
    finishReason: choice.finish_reason ?? null,
    usage,
  };
}
