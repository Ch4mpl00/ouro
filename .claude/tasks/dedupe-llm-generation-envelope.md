# Dedupe the "one traced LLM generation" envelope

**Status:** pending
**Priority:** P2
**Area:** agent / providers + tracing
**Created:** 2026-06-07

## Context

The same envelope — *open a generation observation on a trace scope →
`provider.complete(...)` → close it with `usage` (success) or `ERROR`
(throw)* — is hand-written in THREE places:

- `session.ts` `runUntilSettled` (the per-iteration LLM call)
- `workflow/execute.ts` `execLlmCompose`
- `workflow/compile.ts` `runRetryLoop` (`attempt-N`)

Any change to that envelope (retry on 429, log `finishReason`, tweak
what we record as span output) means editing three call sites that must
stay in sync. The provider abstraction already unified *request shaping +
usage normalization*; this unifies the *tracing envelope* around it.

## Decision: a free function `runGeneration`, NOT a provider method/decorator

We considered folding the span into the provider (so the provider both
shapes the request AND traces it). Rejected — it couples two independent
axes of change onto one unit:

- The **provider** varies by **vendor** (DeepSeek `thinking` +
  `reasoning_content`; cached tokens under `prompt_cache_hit_tokens` vs
  `prompt_tokens_details`). That's legit vendor knowledge.
- The **trace envelope** varies by **observability backend** (Langfuse /
  null / future OTel) and is identical across vendors.

Merging them gives the provider two reasons to change (SRP) and drags a
`TraceContext` dependency into it (DIP) — `providers.test.ts` would then
have to mock the tracer, not just `chat.completions.create`.

Decorator variants were also rejected under the current **explicit-scope**
tracer:

- Widen `complete(params, scope, name, output)` → it stops being a
  `ChatProvider` (not substitutable), so `resolveProvider` can't return
  it; it's just `runGeneration` wearing an object costume + a parallel
  type. No win.
- Keep `complete(params)`, smuggle `scope` through `CompletionParams` →
  true GoF decorator, but trace concerns now live in the provider's
  contract type that every provider/test sees. Same coupling, hidden.
- Ambient context (AsyncLocalStorage / OTel active span) → a clean
  decorator IS possible, but only after rewriting the tracer from
  explicit threaded `scope`/`traceScope` handles to ambient context.
  That sacrifices the explicit nesting sub-agents rely on (a child loop
  nests under the parent's `invoke_sub_agent` span passed in as scope).
  Disproportionate to a 3-call-site dedup.

So: provider answers *"what to send / how to parse the vendor reply"*;
`runGeneration` answers *"how to wrap one call in observability"*. Caller
owns `scope` / `name` / output-rendering because that's caller knowledge
(which step / iteration this is). Composition, not inheritance.

## Acceptance

- New `packages/agent/src/generation.ts` exporting `runGeneration(opts)`:
  opens the generation on `opts.scope`, runs `provider.complete`, closes
  with `usage` on success / `ERROR` + rethrow on failure. Returns the raw
  `CompletionResult`.
- Optional `output?: (r) => unknown` selector (default = whole assistant
  `message`, preserving Session's current trace output); compose +
  compiler pass `(r) => r.message.content ?? ""` to keep their text
  output unchanged.
- Optional `modelParameters` override (default surfaces
  `reasoning_effort` + `thinking`, matching Session today); compose +
  compiler pass their own block.
- All three call sites switch to it. Traces / behaviour observably
  identical (same span names, same usage, same error wrapping at the
  call site — `LlmCallError` for compose, structured `llm_error` for the
  compiler).
- `pnpm typecheck` + tests green.

## Notes

- Helper deliberately stays narrow: ONE generation. Tool dispatch and
  trace-level (whole-run) ERROR markers stay in the AgentLoop / executor.
- Companion cleanup, separate: `Session` class renamed to `AgentLoop` to
  free the word "session" (already overloaded with `sessionId` = the
  trace-grouping unit = the signal). Done ahead of this task.
