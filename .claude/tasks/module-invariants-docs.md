# Module-level invariant docs

**Status:** pending
**Priority:** P3
**Area:** docs
**Created:** 2026-06-01

## Context

Each domain module (currently `news`, embeddings, future
`agent-memory` etc.) has implicit invariants — e.g. NewsModule
assumes one embedding per news_items row; a multi-chunk Chunker
needs a schema change. That rule lives in a code comment inside
`service.buildText`, not at module level. A new contributor reading
`news/module.ts` won't see it.

## Acceptance

- Each `module.ts` opens with a short doc block listing:
  - what this module owns (tables, external resources)
  - what guarantees it makes
  - what it does NOT do (e.g. "doesn't handle multi-chunk
    embeddings — see news_item_chunks task if you need that")
- One paragraph max per item. No re-explaining what the code does.

## Notes

Tied to CLAUDE.md "comments should explain the WHY". Module-level
contracts are exactly the WHY.
