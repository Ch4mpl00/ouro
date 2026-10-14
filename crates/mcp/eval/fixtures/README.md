# RAG eval fixtures

Frozen ground truth for the RAG eval harness. Anything here is a
deliberate snapshot — don't refresh casually, because the labelled
gold/acceptable IDs in `queries.jsonl` point into `corpus.jsonl` by id.

## Files

- **`corpus.jsonl`** — point-in-time snapshot of `news_items` from
  the PG store. One row per line, schema:
  `{id, source, externalId, title, url, body, metadata, postedAt}`.
  Embeddings are NOT included — each eval config re-embeds the
  corpus from text so we can compare embedding models / chunkers
  / `buildText` strategies on the same yardstick.

- **`queries.jsonl`** — labelled queries. One per line:
  `{id, query, reformulation, gold[], acceptable[]}`. `gold` IDs
  must appear in retrieval top-K for a passing run; `acceptable`
  are graded credit (relevant context but not the primary answer).

- **`queries-raw.txt`** — source query list (real Langfuse traces
  + synthetic). The labels in `queries.jsonl` were drafted by
  subagent fan-out (3-way corpus split, then 18 per-query critical
  review passes) — see commit history for the labelling workflow.

## Refreshing

Regenerate `corpus.jsonl`:

```
pnpm eval:snapshot                  # most recent 2000 rows
pnpm eval:snapshot -- --limit 5000  # bigger window
```

**A corpus refresh invalidates the labels.** `queries.jsonl` `gold`
arrays will reference rows that no longer exist or have shifted
meaning. Plan a relabel pass alongside any refresh, and record
baseline numbers before/after so regressions are obvious.