# Sqlite client → factory

**Status:** pending
**Priority:** P2
**Area:** infra / db
**Created:** 2026-06-01

## Context

We converted `db/pg/client.ts` to a `createPgClient()` factory but
`packages/mcp/src/db/client.ts` (sqlite, tokens.db) still exposes a
`getDb()` global. Same for `packages/agent/src/db/client.ts`. This is
inconsistent with our DI rules in CLAUDE.md — a new contributor will
copy the wrong pattern, and existing sqlite-touching modules
(integration_account, telegram_messages, signals, scheduled_tasks)
keep their hidden global coupling.

## Acceptance

- `createSqliteClient({ path }): { db, close }` factory replaces
  `getDb()` / `closeDb()` in both packages.
- All consumers (`services/telegram/storage.ts`,
  `services/gmail/*`, `services/scheduler/*`, `services/signals/*`,
  `services/settings/*` etc.) accept the sqlite handle via DI.
- In-code migrations stay where they are (the factory runs them on
  first connection), no behavioural change.
- `setup` script and CLI scripts switch to the factory.
- `getDb` deleted, no `var __mcp_db` global remaining.

## Notes

This is a bigger sweep than the PG conversion because there are more
modules sitting on top of tokens.db. Worth doing in one PR to avoid
two styles coexisting on the same DB.
