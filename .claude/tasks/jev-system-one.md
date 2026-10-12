# Jev (TypeSafe System One) — typed decisions where we currently spend an LLM turn

**Status:** in-progress (news-digest filter moved to Jev on 2026-10-12, PR feat/jev-decide-filter)
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

## Wire format — settled 2026-10-12

Our OpenRouter account has access. `typesafe/jev-1.13` is rejected on
`/chat/completions` ("is a decisions model … Use the /api/alpha/decisions
endpoint"), so it does **not** ride `ChatProvider`. It lives in
`packages/agent/src/decisions.ts` (`createDecisionClient`, built in the
supervisor when `OPEN_ROUTER_API_KEY` is set, exposed as `engine.decisions`;
model `AGENT_DECISION_MODEL`, default `typesafe/jev-1.13`). No prod path
consumes it yet.

```
POST https://openrouter.ai/api/alpha/decisions
{ "model": "typesafe/jev-1.13",
  "state": string | object | array,
  "questions": {
    "<key>": { "type": "choice", "instructions": "…", "criteria": { "<option>": "<description>", … } },
    "<key>": { "type": "score",  "instructions": "…", "criteria": ["Low", "Normal", "High"] },
    "<key>": { "type": "noul",   "instructions": "<a statement>", "criteria"?: { "true": "…", "false": "…" } } } }
→ { "model": "typesafe/jev-1.13-20260917", "id": "gen-dec-…", "provider": "TypeSafe",
    "answers": {
      "<choice>": { "type": "choice", "choice": "<option>", "probabilities": { "<option>": p }, "confidence": p },
      "<score>":  { "type": "score", "score": 0.37, "legend": { "0": "Low", … }, "probabilities": { "0": p, … }, "confidence": p },
      "<noul>":   { "type": "noul", "noul": p } },
    "usage": { "input_tokens": 544, "output_tokens": 82, "cost": 0.0000228 } }
```

Traps the client normalises away: a score is the probability-weighted mean
level *index* (0…n−1, not 0…1) and its probabilities are keyed by index; a
noul has no `confidence` field at all. Measured from here: ~0.7 s end to end
(OpenRouter reports p50 196 ms server-side), ~$0.00002 per three-question call
on a short Russian message, 32k max prompt tokens. A first Russian probe
classified "напомни завтра в 9 оплатить коммуналку" as `reminder` (p=1) and
"money" at 0.98 — anecdote, not calibration.

## First golden set — 2026-10-12

`packages/agent/eval-fixtures/news-filter/`: the 100 latest prod channel posts
(a 2.7 h window), a rubric distilled from `news-digest.md` (`interests.md`),
reference labels by an Opus 5.5 agent (`opus.json`: 33 keep / 67 skip, 33
marked low-confidence), Jev's answers (`jev.json`), and `report.md`. Rerun:
`tsx packages/agent/src/scripts/news-filter-eval.ts jev|compare`.

- Jev, one `noul` call per post, rubric in the instructions: 5.2 s wall at
  concurrency 8, p50 341 ms, $0.0096 for 100 posts. ~2.3k input tokens per
  call, ~80% of it the rubric repeated each time — batching posts per call is
  the obvious next cost lever.
- ROC AUC 0.91 against all labels, **0.994 against Opus's high-confidence
  ones**: the disagreements are almost all borderline calls.
- Probabilities are compressed (nothing above ~0.8), so 0.5 is the wrong
  cut: p ≥ 0.5 → precision 83%, recall 58%. As a **pre-filter** in front of
  the LLM composer, p ≥ 0.2 drops 42% of posts and loses one keep (a
  low-confidence Lavrov quote).
- Caveats: one reference labeller, one short window, no ФОП posts in it.
- **Batching works** (`--batch N`): rubric once in `state`, one `noul` per
  post. Batch 25 → 4 calls, 0.5 s wall, $0.0020 per 100 posts (5× cheaper),
  AUC 0.919 / 0.995 — no quality loss; p ≥ 0.3 keeps 97% recall and drops 46%.
  Batch 10: AUC 0.914, $0.0025. Order/cross-talk effects not tested yet.

### Filter + dedup in one pass — 2026-10-12

`news-filter-eval.ts pipeline` / `compare-dedup`, gold `dedup-opus.json`
(Opus: 68 events, 16 multi-post covering 48 posts). One request per batch of
25: `keep_n` + `covered_n` (vs previous digest #801) + `dup_n` (choice over the
post's embedding neighbours, cosine ≥ 0.55, top 5, packed into one batch).

- 4 calls, 1.4 s, **$0.0026**, 61k input tokens (filter alone: 47k).
- Filter quality unchanged by the extra questions: AUC 0.911 / 0.998.
- End to end (keep ≥ 0.3, covered ≥ 0.6): 100 → 39 posts to the composer,
  duplicate posts 20 → 6, **event recall 92%** (lost one: the Kremlin's
  "nothing to say on the energy truce" merged into the Peskov briefing — a call
  Opus itself marked borderline). covered ≥ 0.5 also drops Lavrov (p 0.51).
- Dedup clustering is **not better than a plain cosine threshold** here:
  pairwise F1 0.57 (P 74% / R 47%) vs embeddings ≥ 0.6 at 0.56. Candidate
  generation caps recall at 60% (cross-language pairs, same briefing reported
  with different facts).
- "Fullest wins": survivor = Opus's fullest in 14/16 events vs 13/16 for
  longest text. Jev often points both posts of a pair at each other, so the
  "at least as many facts" clause is not really respected.
- **Covered is unmeasured**: the window starts 3.7 h after the digest and Opus
  found 0 covered posts. Needs a window overlapping a digest's sources.

### Shipped: the news digest filters on Jev — 2026-10-12

The digest's map phase (3 × gpt-5.4-mini, each also carrying ~25k tokens of
chat history it was told to ignore) is gone:
`list_news → decide(preset="filter") → one compose:news-digest → send`.

- `decisions.ts` §4 `filter(client, {items, prompt, threshold})`: rules in
  `state` once per batch, one `noul` per item, batches packed by size (≤25
  items, ≤24k chars), all batches concurrently. Default threshold 0.3.
- Workflow step `{kind:"decide", preset:"filter", items, prompt, threshold?, bind}`.
  No skill — `prompt` carries every rule (the planner anchor pins it). No
  fallback: a Jev failure fails the step (`decision_error`).
- Chat history dropped from the digest for now: no dedup against previous
  digests until `covered_n` lands.
- Shipped path on the golden set (`news-filter-eval.ts filter`, items in the
  real `list_news` shape incl. metadata, the anchor's prompt): 4 calls, 0.9 s,
  $0.0024 / 100 posts; AUC 0.886 / 0.991; p ≥ 0.3 → recall 97%, 40% dropped.
  Slightly below the experiment (0.919) — generic question wording + metadata
  noise; recall at the operating point unchanged.
- `OPEN_ROUTER_API_KEY` is now required by the supervisor.

## Integration shape — settled (was the open question)

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

- [x] Access obtained (waitlist or OpenRouter beta) and the real request /
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
