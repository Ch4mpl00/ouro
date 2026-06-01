# Typed errors (transient vs permanent)

**Status:** pending
**Priority:** P2
**Area:** infra / errors
**Created:** 2026-06-01

## Context

Embedding provider and storage layers throw `new Error("…")` —
callers cannot tell a transient OpenAI 503 from a permanent 401, or
a transient pg timeout from a permanent constraint violation.
`embed-backfill` bails after one batch failure precisely because it
can't distinguish them; today that's a feature (avoid burning credits
on a bad key), but a real retry policy needs the discrimination.

## Acceptance

- `class TransientError extends Error` exposed from a shared place.
- Provider wraps OpenAI errors: 408/429/5xx → TransientError, 4xx
  (except 408/429) → plain Error.
- `pg` errors classified via error code (08* → transient, 23* →
  permanent, etc.).
- `embed-backfill` retries TransientError batches with bounded
  back-off (max 3 attempts) and only bails on permanent.

## Notes

Don't reach for `neverthrow` / `Either` types — overkill for this
codebase. A nominal class + helper `isTransient(err)` suffices.
