# Tests foundation

**Status:** pending
**Priority:** P1
**Area:** infra / tests
**Created:** 2026-06-01

## Context

We refactored everything to factory functions + DI, which made the
codebase testable. We have zero tests, so the refactor's main payoff is
sitting unused. Without tests the next big change (chunking, search
quality work, alternate embedding provider) will regress silently.

## Acceptance

- `vitest` installed at repo root, runnable via `pnpm test`.
- Unit tests covering `EmbeddingService` (fake `EmbeddingProvider`,
  fake `EmbeddingRepository`, fake `Chunker`) — happy path, embedding
  skip on `embedding != null`, force-mode, provider-failure path
  returning `failed=N`.
- Unit tests for `performSearch` with a stubbed `EmbeddingRepository`
  and a small drizzle-backed sqlite in-memory (or stub the db call) —
  filter combinations, snippet truncation.
- Tests for `truncateChunker` (edge cases: empty, exactly at limit,
  past limit).
- CI-friendly (no real DB, no real OpenAI key).

## Notes

`vitest` over `jest` — better ESM story, faster, native TS support
matches our `tsx` setup.

For drizzle-backed tests, pglite is the cheapest route if we want
real SQL semantics; otherwise stub the repository at the interface
boundary.
