# Reference-free trajectory judge over Langfuse traces

**Status:** in-progress
**Priority:** P2
**Area:** evals / agent
**Created:** 2026-06-07

## Context

A third judge surface, distinct from the two in [[eval-llm-judge]] (which are
reference-based: a per-result retrieval judge needing gold, and an output judge
needing a hand-written `expected` spec). This one is **reference-free**: it
reads a completed run from Langfuse — input → every RAG query → what came back
(with snippets) → composition → final output — and scores the whole trajectory
against the skill contract, no gold label needed.

Its leverage: because the whole trajectory is in the trace, it judges
coverage / dedup / faithfulness *against what retrieval actually surfaced in
that run* — which gold sets can't (they're about an abstract corpus) and an
output-only judge can't see (it has no steps).

Same judge code serves two surfaces; only the trace source differs:
- **online** — sampled prod traces (digests are low-volume → judge 100%).
  Drift monitoring. ← current focus.
- **offline/CI** — over a frozen Langfuse Dataset (the [[eval-agent-e2e]]
  sandbox). Regression gate. Deferred.

Complements [[eval-foundation]] (deterministic Recall@k on gold), does not
replace it.

## Design decisions (locked this session)

- **Online-first, reference-free.** Observation only — no feedback loop into
  `dreaming` (judge-as-reward = reward hacking).
- **Source = Langfuse API, raw trace.** Not the agent's in-memory store —
  decoupled from the agent, no supervisor edits. Langfuse stores full payloads
  (no truncation), confirmed: a digest trace pulls all 50 search hits with
  snippets. Normalization into a `TrajectoryTranscript` is deferred until token
  cost / reproducibility hurt.
- **Judge model != generator family.** GPT-5.4 judges the DeepSeek composer →
  no self-preference. (Caveat: the planner is also GPT-5.4, so mild
  self-bias on `query_formulation` — watch in calibration.)
- **General engine + contract from the skill**, not a judge-per-scenario. New
  scenario = new skill, judge unchanged. Mirrors the agent's own split
  (compiler + skill). Axes bind to *roles*: `query_formulation` → the
  orchestrator (`planner`); `coverage`/`composition`/`faithfulness` → the
  composer skill. Critical fix (v2 prompt): the judge must NOT penalize the
  composer for orchestration machinery (`send_telegram_message`,
  `get_telegram_chat_history`, tool choice) — that's the planner's job.
- **Hybrid granularity.** One holistic call (the soft axes) + a separate
  faithfulness sub-judge (claim decomposition — vibe-rating faithfulness misses
  fabricated numbers/dates).

## Done (MVP)

Holistic MVP committed `beb0b3a`; faithfulness sub-judge added after, **not yet
committed**.

- `pnpm judge <traceId>` — `packages/agent/src/scripts/judge.ts`.
- `langfuse-api.ts` — shared read client (`api` + `Trace`/`Observation` types +
  `fetchTraceById`), 5xx/network retry (Langfuse Cloud 502s are transient).
  `langfuse-trace.ts` deduplicated onto it.
- `readSkillRaw` in `skills.ts` — raw contract text (live→default) without
  frontmatter validation.
- Holistic axes: `coverage`, `query_formulation`, `composition`; orchestrator
  (`planner`) + composer contracts both fed; structured output; prompt
  versioned (`v2`).
- Faithfulness sub-judge: claim-decomposition, per-claim
  supported/partial/unsupported + evidence (item id), `score =
  (supported + 0.5*partial)/total`, `n/a` on a quiet-day output.
- Validated on 4 prod traces (tech-digest quiet-day, two news-digests). Judge
  **differentiates quality**: news-digest f56a0f (0.56/0.70/0.98) vs 0fa1fc
  (0.23/0.38/0.90). Faithfulness caught real fabrications (assigned outages to a
  district not in any snippet; "жертвы по мосту" with no source).

## Done — A/B model replay (`judge-replay.ts`)

`pnpm tsx packages/agent/src/scripts/judge-replay.ts <trace>` replays a captured
generation's recorded input under a different model. The input already pins
everything but the model (compiler input = the signal; composer input = the
frozen rendered tool results), so no executor / mock-MCP is needed for the
single-LLM-step workflows. Always 5 independent generation samples (one lucky
shot is a poor metric in a non-deterministic system).

- **Test A (`--plan <model>`)** — replay the planner → Workflow JSON, lite
  structural validity. gpt-5.4-mini: 3/5 valid without thinking; `--thinking`
  (reasoning=high) → 5/5 valid, 3/5 recover the deduped
  `get_telegram_chat_history` step, 0 invented steps. Committed `eb2f4ea`.
- **Test B (`--compose <model>`)** — replay the composer → digest text. Pure
  compose-model A/B (tool results frozen). gpt-5.4-mini needs `--thinking`:
  without it +50% length, date confusion (copies the previous digest's date out
  of the dedup context), market-price noise; with it all three gone.
- **Swap-debiased pairwise judge (`--judge`)** — A (original) vs each B sample,
  both in ONE judge call (one salience bar) over the frozen candidates, each
  pair judged in BOTH orders, winner kept only if it survives the swap (a verdict
  that flips with order = position bias → tie). `JUDGE_PAIRS` caps the costly
  swap count; generation stays at `SAMPLES`. See the judge-method finding below.

## Remaining

1. **Calibration (trust gate).** Hand-grade 5–10 traces, measure judge↔human
   agreement per axis (≥80%), held-out split never used for prompt tuning.
   Don't trust the numbers for regressions until this passes. Judge is currently
   harsh on `coverage` (0.23) — confirm that's real, not strictness.
2. **dedup axis** — for non-empty digests (one story across N channels not
   repeated).
3. **Langfuse score ingestion** — write back per-axis: numeric 0..1 +
   categorical label + comment=rationale, stamped `judge_model` /
   `judge_prompt_version`. Alert on a moving average, not a single trace.
4. **Transcript normalization** → `TrajectoryTranscript` (when token cost /
   reproducibility hurt) + deterministic scorers (`search_called`,
   `iter_count`, length bounds, |R\F|). Raw transcript is ~14–30k tokens/trace.
5. **Pairwise `--compare`** — DONE for the compose-model case as the
   swap-debiased `--judge` in `judge-replay.ts` (above). Still to generalize:
   compare two arbitrary traces / surfaces (e.g. semantic vs full-text+LLM
   terms), and raise `JUDGE_PAIRS` / span multiple days for statistical power
   (3 pairs on 1 day is recon, not a verdict). For pure retrieval A/B prefer
   `eval-foundation` Recall@k; judge pairwise is for the end-to-end effect.
6. **Worker** — poll Langfuse by tag+time, filter successful (no `fallback`
   ERROR), dedup by existing score, judge, ingest. Manual → daemon.
7. **Counterfactual `R*`** for `query_formulation` — best-of-k judge-generated
   `Q*` through the same retriever, `asOfISO=signal_time` (the field added this
   session) for point-in-time. Splits "bad formulation" (agent) from "retriever
   can't reach it" (`rag.recall_gap`, not the agent's fault).

## Notes

- **Judge-method finding (deepseek-v4-pro `A` vs gpt-5.4-mini+thinking `B`,
  news-digest f56a0f).** Three judging methods gave three different answers; two
  were artifacts. (1) Naive single-order pairwise: A wins coverage 5:0 — but A
  just sat first more often, position bias inflated it. (2) Pointwise-per-digest
  + penalty arithmetic: B wins 5:0 — critiquing each digest ALONE let the
  salience bar DRIFT between calls (the same ЗАЭС miss was penalized for A yet
  waved through for B sample 5). (3) Swap-debiased pairwise (both digests in one
  call = one bar, each pair judged both orders, flip→tie): they're COMPARABLE —
  mostly tie, the only durable signal toward B (composition 2/3), zero durable A.
  Carry forward: (a) a one-call pairwise keeps the bar consistent; per-digest
  pointwise does not. (b) gpt-5.4's position bias is strong — it almost always
  picks the first-listed, so never trust a single-order pairwise; the swap is
  mandatory. (c) the real A/B gap here is smaller than the judge's own noise →
  needs more pairs/days, not a verdict from one trace.
- **Gemini for the planner (Test A on news-digest f56a0f).** Added a Gemini
  provider (OpenAI-compat endpoint) + 429/5xx retry to `judge-replay`. Finding:
  `gemini-2.5-flash` emits structurally clean workflows (5/5 valid; version /
  bind / input all present, hardcoded timestamps are correct like the original)
  but DROPS the `get_telegram_chat_history` dedup step 10/10 across both thinking
  modes — the skill needs chat history and can't fetch it itself, so digests
  would repeat yesterday's news. `gemini-3-flash-preview` restores the dedup step
  first try (1/1, parallel[list_news+history] → compose{posts,history}).
  `gemini-3.5-flash` (billing key, 10 runs): 10/10 structurally valid, 10/10
  restore the dedup step across both thinking modes, and the raw JSON confirms
  `input.history` is wired into `llm_compose` (dedup actually connected, not just
  the step present) — on par with the gpt-5.4 original, uses `${digest}` like it.
  Bottom line: Gemini-3 gen understands the non-obvious dedup step (3.5: 10/10),
  2.5 does not (0/10) — a clean generation-jump on this task. Also surfaced a
  tooling gap — `printSteps`/`validateWorkflowLite` hide
  version/bind/input, so judging workflow quality needs the raw JSON or the real
  `WorkflowSchema`, not the lite check.
- Judge surfaced two *system* findings (not yet filed): planner casts
  search facets wider than the composer's Interests → noise; `news-digest`
  composer filters weakly. File after calibration confirms the judge isn't
  over-strict.
- Removed the stale live `skills/*` overlay this session (headline-scan
  tech-digest contradicted the search_news pipeline + broke `readSkill` with
  missing frontmatter). System now runs on `skills.default/` only.
