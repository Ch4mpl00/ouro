# Transaction boundaries in news writes

**Status:** pending
**Priority:** P3
**Area:** news / storage
**Created:** 2026-06-01

## Context

`upsertArticleBody` + `embedItems` is a two-step network operation.
If the embedding call fails the body is in PG with `embedding=NULL`,
caught by `pnpm embed:backfill`. That's intentional today.

If a future write path requires "row + embedding land together or
nothing" (e.g. transactional updates to multiple rows that must be
consistently re-embedded), we'll need `db.transaction(tx => ...)`
plumbed through the storage / repo factories.

## Acceptance

(deferred — open this when we have a concrete case)

- `EmbeddingRepository.saveEmbedding` and `NewsStorage.*` accept an
  optional `Tx` parameter.
- drizzle's transaction helper threads through.
- One use-case actually needs it.

## Notes

Don't add transaction support speculatively. Today's eventual-
consistency model (write row → embed → if fail, backfill) is fine for
the loads we have.
