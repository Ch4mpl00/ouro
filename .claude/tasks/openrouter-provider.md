# Evaluate OpenRouter as a provider (opt-in experimentation surface)

**Status:** pending
**Priority:** P3
**Area:** agent / providers
**Created:** 2026-06-21

## Context

We perpetually A/B models (the whole GAIA effort is model-choice-driven;
[[project_deepseek_toolcall_leak]] → "route acting steps to gpt"; presets
already take env overrides `AGENT_{BASE,SMART,SMARTEST,COMPILER}_MODEL`).
Today each provider is wired by hand — `providers/{deepseek,openai,gemini}.ts`,
routed in `engine.resolveProvider(model)` by model-name prefix. Trying a new
model from a different vendor (Claude, Qwen, Llama, …) means a new provider +
key + wiring. OpenRouter is one OpenAI-compatible endpoint + one key that
fronts hundreds of models by string (`anthropic/claude-…`, `deepseek/…`), so
swapping a preset's model becomes a one-line change.

The question raised: should we move to OpenRouter? Conclusion from the
discussion: **NOT a wholesale prod migration — add it as an opt-in
experimentation provider**, keep direct providers as the prod default, and
only promote a specific OpenRouter route once it measures well.

## Why not a wholesale migration (risks specific to us)

- **Prompt-cache is load-bearing.** Planner/compiler economics rest on cache
  (cheap because mostly cached). OpenAI + DeepSeek-direct both cache and we
  already track it (`providers/usage.ts` normalizes `prompt_cache_hit_tokens`
  / `prompt_tokens_details.cached_tokens`). OpenRouter caching is
  routing/backend-dependent and historically less consistent → risk of
  silently losing planner cache and breaking the replan-loop cost model we
  just validated.
- **The DeepSeek tool-call leak is chat-template/backend-specific.** We just
  fixed it (composer base skill, leak 0/39). OpenRouter routes `deepseek/…`
  to one of several hosters (DeepSeek official, Fireworks, Together, Novita…)
  with different templates/tokenizers → the native tool-call markup leak could
  reappear or change shape. Pinning `provider.order` + `allow_fallbacks:false`
  controls it but forfeits the fallback benefit and adds config.
- **Blast radius + latency.** Today a DeepSeek outage hits only `smart`.
  OpenRouter in front of everything = single point of failure + an extra
  network hop.
- **Codex / `code_agent` stays separate** (ChatGPT quota) — out of scope.

## Why still add it (real value)

Frictionless model A/B for a team that does it constantly: one key instead of
three, one usage dashboard, and `AGENT_SMART_MODEL=openrouter/anthropic/claude-…`
makes the bench run through any vendor with zero new code.

## Acceptance

- `providers/openrouter.ts` — a `ChatProvider` against the OpenRouter base
  URL (OpenAI-compatible SDK, same `openai-native-fetch` path), reusing
  `withRetry` and the usage-normalization shape.
- `engine.resolveProvider` recognizes an `openrouter/` model prefix and routes
  to it; built only when `OPENROUTER_API_KEY` is present (optional, like the
  Gemini key today).
- **Prod defaults unchanged** — direct providers stay the default; OpenRouter
  is reached only via an explicit `openrouter/…` model string in an env
  override. No change to the planner/compiler/`smart` prod routes.
- A measurement pass on GAIA through one OpenRouter route, checking the two
  things that actually matter: (a) does prompt-cache still register
  (cache-hit tokens > 0 on the planner), (b) does the DeepSeek leak stay at 0.
  Promote a route to a prod default ONLY if both hold and latency is fine.

## Notes

- Per-provider quirks we already model (DeepSeek `thinking` + non-standard
  `reasoning_effort:"max"`, Gemini's own effort enum) — OpenRouter passes some
  through inconsistently; re-validate `reasoning_effort` handling per target
  model.
- DI seam makes this cheap: ~one new file + one branch in `resolveProvider`;
  no consumer changes (everything already takes a resolved provider).
