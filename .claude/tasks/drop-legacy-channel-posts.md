# Drop legacy sqlite channel_posts table

**Status:** pending
**Priority:** P3
**Area:** db / cleanup
**Created:** 2026-06-01

## Context

When channel posts moved from sqlite (`channel_posts` table in
tokens.db) to PG (`news_items` source='channel'), the sqlite table
stayed in `packages/mcp/data/schema.sql` as a fallback. The one-shot
`pnpm db:migrate:channel-posts` script copies any remaining rows
across. Once prod has been on the new path long enough that the
sqlite table is provably stale, we can drop it.

## Acceptance

- Verify the sqlite `channel_posts` table holds nothing newer than
  the cutover date on prod.
- Remove `channel_posts` CREATE + indexes from
  `packages/mcp/data/schema.sql`.
- `DROP TABLE IF EXISTS channel_posts` migration (in-code, via
  `runMigrations` in `db/client.ts`).
- Delete `pnpm db:migrate:channel-posts` script + workspace entry.

## Notes

Don't rush. Wait at least 2 weeks of clean prod operation on the PG
path. The cost of keeping a dead table is near-zero; the cost of
losing data because we dropped it too early is real.
