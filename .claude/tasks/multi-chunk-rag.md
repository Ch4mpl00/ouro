# Multi-chunk RAG (sliding window)

**Status:** pending
**Priority:** P3
**Area:** news / embeddings
**Created:** 2026-06-01

## Context

Today's `truncateChunker` returns one chunk per news_items row — head
of the text, capped at ~8000 chars. For long HN threads or long Habr
articles that means we silently drop the tail. Retrieval quality
suffers when the relevant point sits past the cutoff.

`Chunker` interface already supports multi-chunk (returns `string[]`),
but the EmbeddingService throws if a chunker produces more than one
chunk — by design, because `news_items` has exactly one embedding
column.

To unlock multi-chunk we need a sibling table that owns the (item_id,
chunk_idx, chunk_text, embedding) tuples and search has to switch to
joining against it.

## Acceptance

- Schema migration adds `news_item_chunks (id, news_item_id FK,
  chunk_idx, body, embedding vector(1536))`.
- `EmbeddingRepository.saveEmbeddings(itemId, chunks)` replaces the
  per-row `saveEmbedding`.
- `EmbeddingService` lifts the single-chunk guard.
- `performSearch` joins chunks → items, returns the top-k items by
  best-chunk distance (or aggregates per item).
- `slidingWindowChunker(size, overlap)` impl in
  `embeddings/chunker.ts`.

## Notes

Only do this when retrieval quality demonstrably suffers — measure
first (find a real query where the relevant chunk lives past 8000
chars), then chunk. Don't pre-emptively complicate the schema.

This is the canonical "multi-chunk strategy" mentioned in the
`truncateChunker` doc.
