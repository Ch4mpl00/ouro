# Agent sqlite migration race across concurrent booters

**Status:** OPEN (latent; self-heals via restart)
**Priority:** P3
**Area:** agent / db / deploy
**Created:** 2026-06-18

## What

Three compose services run `pnpm setup:agent` on boot against the SAME shared
`agent-data` volume (`packages/agent/data/agent.db`): `agent`, `judge-worker`,
and now `improve-worker`. They start concurrently, so on the FIRST boot after a
new Drizzle migration all three read "migration not applied", all three run the
`CREATE TABLE …`, one wins and the others crash with:

```
SqliteError: table `improver_state` already exists
DrizzleError: Failed to run the query 'CREATE TABLE `improver_state` …'
```

Observed on the 2026-06-18 deploy of `0001_improver_state` (Phase 3 п3). It
SELF-HEALS: `restart: unless-stopped` restarts the crashed container, the winner
has by then recorded the migration in `__drizzle_migrations`, so the retry skips
it and boots cleanly. Net effect = one noisy crash + restart per service per new
migration, then steady state. No data loss (CREATE TABLE is the failing op, not
a write).

## Why it matters

- Noisy logs / false-alarm crash on every migration deploy, scaling with the
  number of booters (now 3).
- A migration that is NOT a bare `CREATE TABLE` (e.g. data backfill, or a step
  that partially applies before the conflict) could leave a half-applied state
  the loser's retry won't detect. Today's migrations are all idempotent-ish
  table creates, so we've been lucky.

## Fix options (pick when touched)

1. **Single migrator.** Only ONE service applies migrations (e.g. a one-shot
   `db-migrate` init container, or gate `setup:agent` behind a flag so only the
   `agent` service runs it; the workers just open the db). Cleanest.
2. **Advisory lock / serialize.** Wrap migrate in a cross-process file lock on
   the volume so booters queue.
3. **Tolerate.** Catch "already exists" in the migrate path and treat as applied
   — masks the symptom, not the half-applied risk. Weakest.

Recommendation: option 1 (single migrator init container) — matches the
composition-root discipline and removes the race entirely.

## Context

- Migration apply happens in `packages/agent/src/db/client.ts` `createAgentDb()`
  on every boot; `scripts/setup.ts` is the `setup:agent` entrypoint.
- Compose services that run it: `agent`, `judge-worker`, `improve-worker`
  (`docker-compose.yml`).
