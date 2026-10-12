# Jev (TypeSafe System One) — typed decisions where we currently spend an LLM turn

**Status:** pending (evaluation not started)
**Priority:** P2
**Area:** agent / models
**Created:** 2026-09-20

## Context

TypeSafe AI released **Jev** on 2026-09-15 — the first public "System One"
model. It is not a chat model: you give it program state plus a schema of
typed questions defined in advance, and it answers all of them in one parallel
pass with typed values and calibrated probabilities. It generates no strings
at all.

- 70–500 ms end to end, claimed 40–200× faster than frontier LLMs
- $0.042 / M input tokens, output free
- type errors are structurally impossible (the schema is the output space)
- ceiling of 255 choices per decision; schema must be known up front
- early access by waitlist; founder Diogo Almeida (ex-OpenAI, RLHF/ChatGPT)
- ~32k context per request (secondary sources — verify)

The reason it matters here: several of our LLM turns are not writing tasks at
all, they are classification and scoring wearing a chat interface.

## Where it fits, best first

1. **The news-digest significance filter.** Today the map phase is 3 ×
   gpt-5.4-mini over 48–55k tokens each — $0.152 of a $0.219 run, ~16s. Two
   different jobs live in there: reduce 367 posts to ~20, and write Russian
   prose about the survivors. Only the second needs an LLM. A typed schema
   (category enum, significance score, priority-topic flags for ФОП /
   мобилизація, ad-or-filler flag) covers the first at ~$0.004.
   **Consequence of the 32k context:** the whole window (≈291k chars) does not
   fit one request — this becomes per-post (or ~100-post batch) calls with
   bounded concurrency, not one giant call. Measure wall time before assuming
   it is faster.
2. **Routing in the orchestrator.** `routing.md` spends a full turn of a big
   model deciding digest / topical query / reminder / chit-chat. Calibrated
   probability gives a principled escalation threshold instead of a vibe.
3. **Dedup against chat history** — "was this event already covered?" is the
   failure the digest skill worries about most, and it is a yes/no with a
   confidence.
4. **Judge pre-screen.** `judging.ts` burns codex quota per node. Jev could
   cover cheap axes and flag which nodes deserve the expensive judge. This is
   the one candidate we can measure honestly today: judged corpus + σ noise
   baseline already exist.
5. **Plan-cache validity** — "does this signal match cached plan template X?"
   in ~100 ms, before falling back to the compiler (see the durable-runs
   discussion).

Not a candidate anywhere text is produced: digest composition, recovery
messages, the planner itself.

## Integration shape — open question

If OpenRouter (or TypeSafe) exposes Jev as `/chat/completions` with a JSON
schema, it can ride the existing `ChatProvider` seam and [[openrouter-provider]]
covers it. If the wire shape is genuinely different (typed questions in,
probabilities out), it does **not** fit `ChatProvider` and belongs in its own
`packages/agent/src/decisions.ts` module (factory + DI, one domain one file),
consumed by the workflow executor / orchestrator.

**Nobody has seen the actual wire format yet.** Settle that before designing.

## Availability, as of 2026-09-20

- Direct: early access, waitlist.
- OpenRouter: announced in beta (`typesafe/jev-1.13`, `~typesafe/jev-latest`),
  but **not in the public catalogue** — `GET https://openrouter.ai/api/v1/models`
  returns 446 models and zero `typesafe`/`jev` entries, and the model pages 404
  anonymously. So it is gated; an account with beta access is needed to see the
  real shape.

## Acceptance (evaluation, not adoption)

- [ ] Access obtained (waitlist or OpenRouter beta) and the real request /
      response shape written down here.
- [ ] Offline replay: the 367 posts from Langfuse trace
      `bcd6ebda2eba7db1b3786c4258100d8b` (pull with `pnpm trace … --raw`) scored
      through a Jev schema; compare the kept set against what gpt-5.4-mini's map
      kept and what reached the final digest. Metrics: recall of digest events,
      precision at equal budget, cost, wall time with realistic concurrency.
- [ ] Calibration sanity check on Russian/Ukrainian text — vendor benchmarks are
      almost certainly English.
- [ ] Only then: decide module shape and whether any prod path moves.

## Notes

- Two-week-old vendor. Nothing on the delivery path without a fallback to the
  current LLM step.
- It is another processor for private Telegram content — a separate decision
  from the technical one.
- Because output is free and input is ~1000× cheaper than the map phase, the
  interesting failure mode is not cost but *silent quality loss*: a filter that
  drops the one item the user cared about. The offline replay exists to catch
  exactly that.
