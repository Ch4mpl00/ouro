import type OpenAI from "openai";
import { z } from "zod";
import { Effect } from "effect";
import { toError } from "./errors";
import { traceGenerationEffect } from "./generation";
import { retryOnTransient } from "./providers";
import type { TokenUsage, TraceContext } from "./tracing";

// Typed decisions — classification that does not need a chat turn.
//
// A "System One" model (TypeSafe's Jev, served by OpenRouter) is not a chat
// model: it takes program state plus a schema of typed questions and answers
// all of them in one parallel pass with calibrated probabilities, ~200 ms, no
// generated text. It cannot ride the ChatProvider seam — OpenRouter rejects it
// on /chat/completions and serves it on its own endpoint — so it gets its own
// module. Rationale and the evaluation plan: .claude/tasks/jev-system-one.md.
//
// Contents:
//   1. Wire contract — what POST /api/alpha/decisions takes and returns
//   2. Questions     — choice / score / noul builders, each owning its parser
//   3. Client        — createDecisionClient: retry, deadline, trace generation
//   4. Presets       — task shapes over the client: filter
//
// Type safety is end to end: the answer type of every question is derived from
// the question itself, so `decision.answer("intent").choice` is the union of
// that question's option keys, and an answer the API returns outside the
// declared options fails loudly at the call instead of flowing on as a string.

// ─── 1. Wire contract ────────────────────────────────────────────────

// Not under the chat baseURL (…/api/v1): the decisions API is versioned
// separately and still alpha. Verified against the live API on 2026-10-12.
export const OPENROUTER_DECISIONS_URL = "https://openrouter.ai/api/alpha/decisions";

// A decision is ~200 ms p50, ~400 ms p99. The generic 10-minute generation
// deadline would let a hung request stall its caller far past the point where
// falling back to an LLM would have been faster.
export const DEFAULT_DECISION_TIMEOUT_MS = 15_000;

// The API's ceiling per choice question.
export const MAX_CHOICE_OPTIONS = 255;

type WireQuestion =
  | { type: "choice"; instructions: string; criteria: Record<string, string> }
  | { type: "score"; instructions: string; criteria: string[] }
  | { type: "noul"; instructions: string; criteria?: { true: string; false: string } };

// The model is the state's only reader, so any JSON shape works: a string, or
// an object / array when the context has several parts.
export type DecisionState = string | Record<string, unknown> | unknown[];

interface WireRequest {
  model: string;
  state: DecisionState;
  questions: Record<string, WireQuestion>;
}

const RawResponse = z.object({
  model: z.string(),
  id: z.string().optional(),
  answers: z.record(z.unknown()),
  usage: z
    .object({ input_tokens: z.number(), output_tokens: z.number(), cost: z.number().optional() })
    .optional(),
});

// Per-type answers as the wire sends them. Two traps the normalised shapes
// below remove: a score's probabilities are keyed by level INDEX ("0", "1",
// …), not label; and a noul has no `confidence` at all — absent, not null.
const RawChoice = z.object({
  type: z.literal("choice"),
  choice: z.string(),
  probabilities: z.record(z.number()),
  confidence: z.number(),
});
const RawScore = z.object({
  type: z.literal("score"),
  score: z.number(),
  probabilities: z.record(z.number()),
  confidence: z.number(),
});
const RawNoul = z.object({ type: z.literal("noul"), noul: z.number() });

// ─── 2. Questions ────────────────────────────────────────────────────

export class DecisionError extends Error {
  override readonly name = "DecisionError";
}

// A question carries its own wire form and the parser for its answer, so the
// answer type travels with the question rather than living in a parallel map.
export interface Question<A> {
  readonly wire: WireQuestion;
  parse(raw: unknown): A;
}

export interface ChoiceAnswer<C extends string> {
  choice: C;
  // Probability of the picked option.
  confidence: number;
  probabilities: ReadonlyMap<C, number>;
}

export interface ScoreAnswer<L extends string> {
  // The most probable level.
  level: L;
  // Probability-weighted mean level INDEX: 0 … levels.length − 1, not 0 … 1.
  score: number;
  // Probability of `level`.
  confidence: number;
  // Aligned with the levels as declared.
  probabilities: readonly number[];
}

export interface NoulAnswer {
  // P(statement is true).
  probability: number;
}

// Pick one of a fixed set. Keys are what comes back; values describe each
// option to the model.
export function choice<const C extends Record<string, string>>(
  instructions: string,
  options: C,
): Question<ChoiceAnswer<Extract<keyof C, string>>> {
  type Option = Extract<keyof C, string>;
  const count = Object.keys(options).length;
  if (count < 2 || count > MAX_CHOICE_OPTIONS) {
    throw new DecisionError(`choice needs 2–${MAX_CHOICE_OPTIONS} options, got ${count}`);
  }
  const isOption = (key: string): key is Option => Object.hasOwn(options, key);
  return {
    wire: { type: "choice", instructions, criteria: options },
    parse(raw) {
      const answer = RawChoice.parse(raw);
      if (!isOption(answer.choice)) {
        throw new DecisionError(`choice "${answer.choice}" is not one of the declared options`);
      }
      const probabilities = new Map<Option, number>();
      for (const [key, p] of Object.entries(answer.probabilities)) {
        if (isOption(key)) probabilities.set(key, p);
      }
      return { choice: answer.choice, confidence: answer.confidence, probabilities };
    },
  };
}

// Rate on ordered levels, lowest first.
export function score<const L extends readonly [string, string, ...string[]]>(
  instructions: string,
  levels: L,
): Question<ScoreAnswer<L[number]>> {
  return {
    wire: { type: "score", instructions, criteria: [...levels] },
    parse(raw) {
      const answer = RawScore.parse(raw);
      const probabilities = levels.map((_, i) => answer.probabilities[String(i)] ?? 0);
      let best = 0;
      probabilities.forEach((p, i) => {
        if (p > (probabilities[best] ?? 0)) best = i;
      });
      const level = levels[best];
      if (level === undefined) throw new DecisionError("score answer has no levels");
      return { level, score: answer.score, confidence: probabilities[best] ?? 0, probabilities };
    },
  };
}

// Yes/no as a probability. Phrase `statement` as a claim, not a question
// ("The customer threatens to cancel."); `labels` optionally names the sides.
export function noul(
  statement: string,
  labels?: { true: string; false: string },
): Question<NoulAnswer> {
  return {
    wire: { type: "noul", instructions: statement, ...(labels ? { criteria: labels } : {}) },
    parse(raw) {
      return { probability: RawNoul.parse(raw).noul };
    },
  };
}

// ─── 3. Client ───────────────────────────────────────────────────────

export interface DecideRequest<A extends Record<string, unknown>> {
  state: DecisionState;
  questions: { [K in keyof A]: Question<A[K]> };
  // Every decision is a generation in the caller's trace, like an LLM call.
  scope: TraceContext;
  // Observation name in the trace. Default "decision".
  name?: string;
  signal?: AbortSignal;
  timeoutMs?: number;
}

export interface Decision<A extends Record<string, unknown>> {
  answer<K extends keyof A & string>(key: K): A[K];
  // Dated snapshot that actually answered, e.g. "typesafe/jev-1.13-20260917".
  model: string;
  id?: string;
  usage?: TokenUsage;
  costUsd?: number;
}

export interface DecisionClient {
  readonly model: string;
  decide<A extends Record<string, unknown>>(req: DecideRequest<A>): Promise<Decision<A>>;
}

export interface DecisionClientDeps {
  // The OpenRouter client (createOpenRouterClient): its key, fetch and error
  // types. The decisions URL is absolute, so its chat baseURL is not used.
  client: OpenAI;
  model: string;
}

function wireQuestions(questions: Readonly<Record<string, Question<unknown>>>): Record<string, WireQuestion> {
  return Object.fromEntries(Object.entries(questions).map(([key, q]) => [key, q.wire]));
}

// Parse every answer up front, so a malformed or missing one fails inside the
// generation (marked ERROR in the trace) rather than at whichever consumer
// reads it first.
function checkAnswers(
  questions: Readonly<Record<string, Question<unknown>>>,
  answers: Record<string, unknown>,
): void {
  for (const [key, q] of Object.entries(questions)) {
    try {
      q.parse(answers[key]);
    } catch (error) {
      const reason = error instanceof Error ? error.message : String(error);
      throw new DecisionError(`answer "${key}": ${reason}`, { cause: error });
    }
  }
}

function toUsage(usage: z.infer<typeof RawResponse>["usage"]): TokenUsage | undefined {
  if (!usage) return undefined;
  return { input: usage.input_tokens, output: usage.output_tokens, total: usage.input_tokens + usage.output_tokens };
}

export function createDecisionClient(deps: DecisionClientDeps): DecisionClient {
  const { client, model } = deps;

  return {
    model,
    decide(req) {
      const body: WireRequest = { model, state: req.state, questions: wireQuestions(req.questions) };
      const call = (signal: AbortSignal) =>
        client.post<WireRequest, unknown>(OPENROUTER_DECISIONS_URL, { body, signal, maxRetries: 0 });

      const effect = traceGenerationEffect({
        scope: req.scope,
        observation: { name: req.name ?? "decision", model, input: { state: req.state, questions: body.questions } },
        timeoutMs: req.timeoutMs ?? DEFAULT_DECISION_TIMEOUT_MS,
        run: async (signal) => {
          const raw = RawResponse.parse(
            await retryOnTransient(call, {
              signal,
              // Same visibility contract as the chat providers' withRetry.
              onRetry: ({ attempt, status, delayMs }) => {
                console.warn(
                  `[retry] decisions/${model} attempt ${attempt} failed (status=${status ?? "?"}), retrying in ${delayMs}ms`,
                );
                req.scope.event({
                  name: "llm_retry",
                  level: "WARNING",
                  metadata: { provider: "openrouter", model, attempt, status: status ?? null, delay_ms: delayMs },
                });
              },
            }),
          );
          checkAnswers(req.questions, raw.answers);
          return raw;
        },
        describe: (raw) => ({
          output: raw.answers,
          usage: toUsage(raw.usage),
          metadata: { decision_model: raw.model, decision_id: raw.id, cost_usd: raw.usage?.cost },
        }),
      });

      return Effect.runPromise(effect, { signal: req.signal }).then((raw) => ({
        answer: (key) => req.questions[key].parse(raw.answers[key]),
        model: raw.model,
        id: raw.id,
        usage: toUsage(raw.usage),
        costUsd: raw.usage?.cost,
      }));
    },
  };
}

// ─── 4. Presets ──────────────────────────────────────────────────────

// filter: keep the items a prompt's rules let through.
//
// Items go out in batches: the rules ride in `state` once per batch, each item
// gets one noul. Measured on the news golden set (eval-fixtures/news-filter):
// batches of 25 cost 5× less than one item per call with no quality loss, and
// Jev's probabilities are compressed (nothing above ~0.8), so the default cut
// is 0.3, not 0.5 — 97% recall of what Opus kept, 46% of posts dropped.
//
// Batch size is bounded by the API's 32k prompt tokens, not by item count, so
// batches are packed by serialized size. Jev's tokenizer runs ~1.2 chars per
// token on Cyrillic, so 24k chars of items leaves room for the rules.
// Concurrency: our OpenRouter key has no request-rate limit (rate_limit -1),
// so every batch of a run goes out at once; the cap only exists because
// orchestration must be bounded. A failed batch fails the whole filter.

export const FILTER_THRESHOLD = 0.3;
export const FILTER_MAX_BATCH_ITEMS = 25;
export const FILTER_MAX_BATCH_CHARS = 24_000;
export const FILTER_CONCURRENCY = 16;

export interface FilterRequest<T> {
  items: readonly T[];
  // The rules. Everything the model knows about what passes is in here.
  prompt: string;
  threshold?: number;
  scope: TraceContext;
  signal?: AbortSignal;
}

export interface FilterResult<T> {
  // The items that passed, in input order.
  kept: T[];
  // P(passes) per input item, aligned with `items`.
  probabilities: number[];
}

// Consecutive runs of item indices, each under both batch limits. An item
// bigger than the char budget gets a batch of its own (the API rejects it if
// it really exceeds the prompt cap — loudly, as a 400).
function packFilterBatches(items: readonly unknown[]): number[][] {
  const batches: number[][] = [];
  let current: number[] = [];
  let chars = 0;
  items.forEach((item, i) => {
    const size = JSON.stringify(item)?.length ?? 0;
    if (current.length > 0 && (current.length >= FILTER_MAX_BATCH_ITEMS || chars + size > FILTER_MAX_BATCH_CHARS)) {
      batches.push(current);
      current = [];
      chars = 0;
    }
    current.push(i);
    chars += size;
  });
  if (current.length > 0) batches.push(current);
  return batches;
}

export async function filter<T>(decisions: DecisionClient, req: FilterRequest<T>): Promise<FilterResult<T>> {
  const threshold = req.threshold ?? FILTER_THRESHOLD;
  const batches = packFilterBatches(req.items);
  const span = req.scope.span({
    name: "decide:filter",
    kind: "chain",
    input: { items: req.items.length, batches: batches.length, threshold, prompt: req.prompt },
    metadata: { decision_model: decisions.model },
  });

  const decideBatch = async (batch: number[], signal: AbortSignal): Promise<Array<[number, number]>> => {
    const questions: Record<string, Question<NoulAnswer>> = {};
    batch.forEach((_, k) => {
      questions[`item_${k + 1}`] = noul(`Элемент №${k + 1} из items проходит правила из rules.`, {
        true: "Проходит",
        false: "Не проходит",
      });
    });
    const decision = await decisions.decide({
      state: { rules: req.prompt, items: batch.map((i, k) => ({ n: k + 1, item: req.items[i] })) },
      questions,
      scope: span,
      name: "decide:filter:batch",
      signal,
    });
    return batch.map((i, k) => [i, decision.answer(`item_${k + 1}`).probability]);
  };

  try {
    const answered = await Effect.runPromise(
      Effect.forEach(
        batches,
        (batch) => Effect.tryPromise({ try: (signal) => decideBatch(batch, signal), catch: toError }),
        { concurrency: FILTER_CONCURRENCY },
      ),
      { signal: req.signal },
    );
    const probabilities = req.items.map(() => 0);
    for (const [i, p] of answered.flat()) probabilities[i] = p;
    const kept = req.items.filter((_, i) => (probabilities[i] ?? 0) >= threshold);
    span.end({ output: { kept: kept.length, of: req.items.length, probabilities } });
    return { kept, probabilities };
  } catch (error) {
    const message = toError(error).message;
    span.end({ level: "ERROR", statusMessage: message, output: { error: message } });
    throw error;
  }
}
