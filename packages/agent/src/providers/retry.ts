import OpenAI from "openai";
import type { ChatProvider, CompletionParams, CompletionResult } from "./types";

// Transient-failure retry, factored OUT of individual providers. Retrying is
// a cross-cutting reliability policy, not a property of one endpoint: the
// workflow compiler is on the hot path no matter which provider its model
// routes to (AGENT_COMPILER_MODEL switches the route silently), so the
// engine wraps EVERY provider with `withRetry` at startup. 429 (rate limit)
// and 5xx (overload / transient server error) back off and retry; any other
// 4xx is permanent and rethrows immediately.
//
// Visibility contract: a retry must never look like one slow call. When the
// caller passes `CompletionParams.trace`, every retry attempt emits a
// WARNING `llm_retry` event on that scope — attempt number, HTTP status and
// backoff delay land in the Langfuse trace right next to the generation
// they delayed. Without a trace (scripts), retries still go to stderr.

export interface RetryInfo {
  // 1-based number of the attempt that just FAILED.
  attempt: number;
  // HTTP status of the failure, when the error was an APIError.
  status?: number;
  delayMs: number;
}

export interface RetryOpts {
  // Retries after the initial attempt. Default 4 → 2s,4s,8s,16s backoff.
  maxRetries?: number;
  baseDelayMs?: number;
}

// Low-level helper for callers that work with a raw OpenAI client instead
// of a ChatProvider (e.g. the judge-replay script).
export async function retryOnTransient<T>(
  fn: () => Promise<T>,
  opts: RetryOpts & { onRetry?: (info: RetryInfo) => void } = {},
): Promise<T> {
  const maxRetries = opts.maxRetries ?? 4;
  const baseDelayMs = opts.baseDelayMs ?? 2000;
  for (let attempt = 0; ; attempt++) {
    try {
      return await fn();
    } catch (err) {
      const status = err instanceof OpenAI.APIError ? err.status : undefined;
      const retryable = status === 429 || (status !== undefined && status >= 500);
      if (!retryable || attempt >= maxRetries) throw err;
      const delayMs = baseDelayMs * 2 ** attempt;
      opts.onRetry?.({ attempt: attempt + 1, status, delayMs });
      await new Promise((r) => setTimeout(r, delayMs));
    }
  }
}

export function withRetry(provider: ChatProvider, opts: RetryOpts = {}): ChatProvider {
  return {
    kind: provider.kind,
    complete(params: CompletionParams): Promise<CompletionResult> {
      return retryOnTransient(() => provider.complete(params), {
        ...opts,
        onRetry: ({ attempt, status, delayMs }) => {
          console.warn(
            `[retry] ${provider.kind}/${params.model} attempt ${attempt} failed` +
              ` (status=${status ?? "?"}), retrying in ${delayMs}ms`,
          );
          params.trace?.event({
            name: "llm_retry",
            level: "WARNING",
            metadata: {
              provider: provider.kind,
              model: params.model,
              attempt,
              status: status ?? null,
              delay_ms: delayMs,
            },
          });
        },
      });
    },
  };
}
