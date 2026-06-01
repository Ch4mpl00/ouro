# Shared infra module

**Status:** pending
**Priority:** P2
**Area:** infra / DI
**Created:** 2026-06-01

## Context

CLAUDE.md says "no global handles in business code", but
`embeddings/provider.ts` still has `getDefaultOpenAIProvider()` — a
lazy global. It works because there's exactly one consumer
(news-module), but the moment a second domain wants embeddings it
either reuses this global (rule violation) or creates a second OpenAI
client (waste, and divergent rate-limit budgets).

## Acceptance

- `createInfraModule({ config }): InfraModule` returning
  `{ openaiProvider, logger, clock }` (and other future shared
  singletons).
- `getDefaultOpenAIProvider` deleted from `embeddings/provider.ts`.
- `createEmbeddingsModule({ provider, repo, chunker? })` takes the
  provider explicitly (already the case — just no longer optional).
- `server.ts main()`:
  ```ts
  const infra = createInfraModule({ config });
  const news = createNewsModule({ db: pg.db, infra });
  ```

## Notes

`clock: () => Date` is a useful add at the same time — makes any
time-sensitive code testable without `vi.useFakeTimers`.

This task should land after centralized-config and injectable-logger
(both feed into the infra module).
