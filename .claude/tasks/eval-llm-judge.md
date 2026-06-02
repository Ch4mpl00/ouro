# LLM-as-judge layer for RAG + digest output

**Status:** pending
**Priority:** P2
**Area:** evals / RAG
**Created:** 2026-06-01

## Context

[[eval-foundation]] gives us retrieval metrics (Recall@k, MRR) against
hand-labeled gold sets. That covers ~80% of what we need to detect
regressions. The remaining 20% are:

1. **Precision on the long tail of borderline results.** Hand-labelled
   gold is "yes/no per item we thought to check". An LLM-judge can rate
   every top-N result on every query — cheaper than scaling the human
   labelling.

2. **Composition quality.** Retrieval can be perfect and digest output
   still wrong (hallucinated numbers, missed cross-references against
   chat history, format drift, wrong tone for news-query vs news-digest).
   Retrieval metrics don't see any of that.

Judge belongs *after* foundation, not instead of it. Foundation gives
the deterministic numbers; judge gives the qualitative scoring on
non-determministic outputs.

## Acceptance

### Layer 1 — Retrieval judge

- `pnpm eval:judge-retrieval --config <name>` runs over the same
  golden queries from [[eval-foundation]]:
  - For each (query, retrieved top-N) → LLM rates each result:
    `relevant` / `tangentially related` / `off-topic`.
  - Aggregate: Precision@k (relevant / k), and a "graded relevance"
    score (relevant=1, tangential=0.3, off=0).
- Judge is Claude Sonnet 4.6 or Opus 4.7 — NOT the same family as the
  generation model (DeepSeek), to avoid same-model self-bias.
- Prompt is in `packages/mcp/src/eval/judge/retrieval.md` so it can be
  iterated independently of code.

### Layer 2 — Output judge

- `pnpm eval:judge-output --scenario <name>` runs end-to-end:
  - Plays back a saved Telegram signal (`scenarios/*.json`) through
    the full supervisor → produces a composed reply.
  - LLM judges the reply against the scenario's `expected` spec:
    - Factual: does any sentence contradict the corpus snapshot?
    - Coverage: are the top-2 critical events from gold mentioned?
    - Format: matches the skill's output template (e.g. news-query
      = no `📰` header, conversational tone)?
    - Hallucinated specifics (numbers / dates / locations not in
      retrieved snippets)?
  - Output: per-scenario pass/fail + per-rubric scores.

### Calibration

- 10 hand-graded examples per judge layer. Run the judge on those.
  Agreement target: ≥80% with the human grade. If lower, iterate
  the judge prompt before trusting it on new data.

### Self-consistency

- Each judge call run 3× with temperature 0.3; majority vote.
  Reduces flapping on borderline cases.

## Notes

- **Cost.** 25 queries × 10 retrieved items × 3 votes × ~500 tokens
  per judgment = ~375k tokens per layer-1 run. Sonnet 4.6 ≈ $1.10
  per run. Run on PRs that touch RAG, not every commit.

- **Why graded, not binary.** "tangentially related" is the most
  common failure mode of a small embedding model — top-5 has 3
  perfect hits + 2 same-channel-different-event posts. Binary
  metrics flatten that signal.

- **Judge prompt prior art.** Anthropic's Constitutional AI evals,
  RAGAS faithfulness / relevance prompts. Start with RAGAS-style,
  customize per our schema.

- **Output scenarios.** Save full Telegram signal JSONs as fixtures
  (input text, chat history, watermark) so end-to-end replay is
  deterministic. Same snapshot principle as [[eval-foundation]]
  fixture.

- **Anti-bias when iterating.** Don't tune the judge against the
  test set — use a separate "validation" split of 5 examples never
  shown during prompt iteration.

- **What this does NOT do.** It doesn't replace foundation metrics —
  use both. Recall@k catches "embedding model is broken"; judge
  catches "embedding is fine but composition lies".
