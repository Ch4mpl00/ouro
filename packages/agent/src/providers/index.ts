export type {
  ChatProvider,
  CompletionParams,
  CompletionResult,
  ProviderKind,
} from "./types";
export { createOpenAiProvider } from "./openai";
export { createDeepseekProvider } from "./deepseek";
export { createGeminiProvider } from "./gemini";
export { retryOnTransient, withRetry, type RetryInfo, type RetryOpts } from "./retry";

// OpenAI-compatible endpoints of the non-OpenAI providers. One definition
// for every place that constructs a client (engine, replay scripts).
export const DEEPSEEK_BASE_URL = "https://api.deepseek.com";
export const GEMINI_BASE_URL =
  "https://generativelanguage.googleapis.com/v1beta/openai/";
