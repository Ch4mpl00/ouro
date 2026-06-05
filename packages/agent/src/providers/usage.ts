import type { CompletionUsage } from "openai/resources/completions";
import type { TokenUsage } from "../tracing";

// Read a numeric field that may not exist on the typed shape (e.g. DeepSeek's
// `prompt_cache_hit_tokens`, which the OpenAI SDK's CompletionUsage doesn't
// declare). Widening to a record is safe; the runtime typeof guard keeps it
// honest — no narrowing cast to silence the compiler.
export function readNumberField(obj: unknown, key: string): number | undefined {
  if (!obj || typeof obj !== "object") return undefined;
  const v = (obj as Record<string, unknown>)[key];
  return typeof v === "number" ? v : undefined;
}

// OpenAI: cached-prompt tokens live under prompt_tokens_details.cached_tokens.
export function normalizeOpenAiUsage(u: CompletionUsage | undefined): TokenUsage | undefined {
  if (!u) return undefined;
  return {
    input: u.prompt_tokens,
    output: u.completion_tokens,
    total: u.total_tokens,
    cached: u.prompt_tokens_details?.cached_tokens,
  };
}

// DeepSeek: cache hits are reported on bespoke fields
// (prompt_cache_hit_tokens / prompt_cache_miss_tokens) the SDK type omits.
export function normalizeDeepseekUsage(u: CompletionUsage | undefined): TokenUsage | undefined {
  if (!u) return undefined;
  return {
    input: u.prompt_tokens,
    output: u.completion_tokens,
    total: u.total_tokens,
    cached: readNumberField(u, "prompt_cache_hit_tokens"),
  };
}
