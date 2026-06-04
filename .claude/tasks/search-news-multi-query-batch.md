# search_news: batch multi-query with cross-query dedup

**Status:** pending
**Priority:** P2
**Area:** packages/mcp (news / RAG)
**Created:** 2026-06-04

## Context

Today `search_news` takes one `query` per call. A topical ask often
decomposes into several facets ("что говорил Маск про Tesla И про
SpaceX И про политику") — and the workflow compiler can only express
that as one blurred query or as N separate `tool` steps, each a round
trip + its own embed call, with results the caller then has to merge by
hand.

We want: send a **pack of queries in one request**, MCP runs each
independently against the vector store, then **dedups similar results**
across the union before returning. This raises recall for multi-faceted
asks and collapses the N-step pattern into one tool call.

The pieces already exist:
- `search()` in `packages/mcp/src/services/news/core/repository.ts`
  embeds one query, runs a cosine-distance search with a 2× pool, dedups
  within that single result set via `dedupByPairwiseCosine(rows, getEmb,
  threshold)`, and returns top-`k`.
- `dedupByPairwiseCosine` + `DEFAULT_DEDUP_THRESHOLD` (same file's
  imports) already do embedding-space near-duplicate collapsing — they
  just need to run over the *union* of per-query rows instead of one
  query's rows.
- The tool surface is `search_news` in `packages/mcp/src/tools/news.ts`.

## Acceptance

Done when:

1. `search_news` accepts a batch of queries — `queries: string[]`
   (1–N, cap N ~8) alongside the existing single `query`. Keep `query`
   working for backward compat (accept exactly one of `query` /
   `queries`; reject both-empty). Filters (`source`, `sinceISO`,
   `untilISO`, `channel`) and `k` apply to the whole batch.
2. The repository gains a batch search path (e.g. `searchMany(queries,
   opts)` or `search` accepting `string[]`) that:
   - embeds all queries — **one** provider batch-embed call, not N (check
     `EmbeddingService.embed` for an existing batch form; add one if
     missing rather than looping per query),
   - runs the per-query vector search (each query keeps its own 2× pool),
   - **merges** the rows into one set keyed by `news_items.id`; when an
     item is retrieved by several queries, keep its **minimum** distance,
   - dedups the merged set in embedding space with
     `dedupByPairwiseCosine` (cross-query near-duplicate collapse, reusing
     the existing helper + `dedupThreshold`),
   - returns the top-`k` survivors ordered by that min distance.
3. Result shape per item is unchanged (`id, source, title, url, snippet,
   posted_at, distance, metadata`). `distance` = the best (min) distance
   across the queries that surfaced it. Optionally add `matchedQueries`
   (indices/strings) so the caller can see why an item came back —
   decide during impl, don't block on it.
4. Tool description tells the compiler when to batch vs. single, and that
   results are already de-duplicated across queries (so it shouldn't
   re-dedup or re-query per facet).
5. `planner.md` (skills.default) updated: for multi-facet topical asks,
   prefer one `search_news` with `queries: [...]` over N separate
   `tool` steps.
6. Tests cover: union of overlapping result sets keeps the min-distance
   row once; cross-query near-duplicates collapse; `k` is the post-dedup
   cap; single-`query` path still works; filters apply across the batch.
7. `pnpm typecheck` green; MCP tests green.

## Notes

- **Ranking across queries is the one real design call.** Min-distance
  is the simple default (an item is "good" if it's close to *any* facet).
  Alternatives: sum/mean of distances (rewards items relevant to several
  facets), or round-robin interleave per query (guarantees each facet is
  represented in the top-`k`). Min-distance first; revisit if the RAG
  eval shows one facet starving the others.
- **Pool sizing:** with N queries each pulling `max(k*2, 30)`, the merged
  pool is up to `N * pool` before dedup. Fine for N≤8; keep the DB-side
  `.limit(poolSize)` per query so no single query scans the table.
- Stays entirely MCP-side — the agent still calls one tool; no executor /
  workflow-DSL change needed. A workflow `tool` step just passes
  `queries: ["${a}", "${b}"]`.
- Don't fold this into `list_news` — that one is chronological, no
  query vector. This is purely a `search_news` (semantic) concern.
- Possible follow-up (separate task): same batch shape for the agent-side
  fan-out in digests, if it turns out useful beyond ad-hoc search.
