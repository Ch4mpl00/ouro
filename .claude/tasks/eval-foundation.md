# RAG eval foundation — golden set + harness + metrics

**Status:** pending
**Priority:** P1
**Area:** evals / RAG
**Created:** 2026-06-01

## Context

We're making structural changes to RAG (dropped ivfflat index, removed
chat_title from embedding text, switched tech-digest to 3-query
pre-filter, split news-digest from news-query). Each change "looks
right" via 5-query eyeballing, but we have no way to:

- Detect regressions when the next "improvement" silently breaks
  retrieval on tomorrow's corpus.
- Compare embedding models / chunkers / `buildText` strategies on the
  same yardstick.
- Prove that a specific change (e.g. switching to text-embedding-3-large
  at 3072 dims) is worth the cost.

The eyeballing also doesn't scale — labelling 25 queries by hand once
is cheaper than eyeballing the same 5 queries after every commit.

## Acceptance

1. **Golden set fixture** at `packages/mcp/src/eval/fixtures/`:
   - `corpus.jsonl` — point-in-time snapshot of `news_items`
     (id, source, title, body, metadata, postedAt). Embeddings NOT
     included — each config re-embeds the fixture.
   - `queries.jsonl` — labeled queries:
     ```json
     {
       "id": "q-001",
       "query": "шо там Одесса?",
       "reformulated": "Одесса Одесская область обстрелы прилёты...",
       "gold": [927, 36, 82],
       "notes": "ground truth IDs from corpus.jsonl; acceptable
                  but not gold can go in `acceptable` (graded)."
     }
     ```
   - 15–25 queries spanning: regional ("Одесса", "ПМР"), persona
     ("Трамп", "Зеленский"), tech ("OpenAI", "Anthropic"), thematic
     ("ФОП", "ухилянти", "санкции"), tail ("Сирия", "Газа").

2. **Eval harness** `packages/mcp/src/eval/harness.ts`, runnable as
   `pnpm eval:rag --config <path>`:
   - Loads corpus.jsonl + queries.jsonl.
   - Loads config JSON: `{embedModel, dimensions, chunker, buildText}`.
   - Embeds corpus once per config (cache result keyed by config hash
     → `eval/cache/<hash>.jsonl`).
   - For each query: re-embed (per config), cosine vs cached corpus
     vectors, top-N.
   - Computes per-query and aggregate: Recall@5, Recall@10, MRR,
     mean distance to first gold.
   - Markdown report side-by-side comparing N configs.

3. **Baseline run** with current production config logged
   (`text-embedding-3-small`, 1536 dims, truncateChunker, title+body
   buildText). Numbers committed alongside the fixture so future
   PR'ы видят regression.

4. **One side-by-side comparison** as proof of concept: baseline vs
   one variant (e.g. text-embedding-3-large at 1536 dims).

## Notes

- **Labelling bottleneck.** ~5–10 min per query × 25 queries ≈ 2–4h
  human time. Use Langfuse trace log of past `telegram` signals as a
  query source (real user phrasing > invented). For each query: run
  `search_news` against the live store, glance at top-15, mark which
  IDs are actually relevant. Save as `gold` array.

- **Corpus drift.** Snapshot must be a static file — running eval
  against the live PG defeats the point (numbers change daily). When
  the schema or fixture format evolves, refresh the snapshot
  consciously, document why in commit message.

- **Configs are JSON, not code.** Eval harness reads
  `packages/mcp/src/eval/configs/<name>.json`. Versioned in git.

- **Cost.** Per-config corpus embed: ~850 rows × ~300 tokens avg ≈
  255k tokens. text-embedding-3-small = $0.005, 3-large = $0.033.
  Cheap.

- **What this does NOT cover** (separate tasks):
  - Composition / digest quality (output, not retrieval) →
    [[eval-llm-judge]].
  - Multi-chunk strategies (current schema is 1:1 row:vector) →
    [[multi-chunk-rag]].

- **Where to put the fixture.** `packages/mcp/src/eval/fixtures/` —
  src/ so TS imports work, but excluded from the production bundle
  via a separate tsconfig include if needed. Or `.claude/eval/` if
  we want it truly separate from the package — decide on PR.
