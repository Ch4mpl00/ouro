# News chronological query does not use its timestamp index

**Status:** done
**Priority:** P2
**Area:** MCP news repository / Postgres
**Created:** 2026-10-02

## Context

The first Rust production checks found `list_news(limit=1)` taking 25–28 s
on 108,287 rows. EXPLAIN uses a parallel sequential scan and sort for
`ORDER BY posted_at DESC LIMIT 2`. The existing index is
`posted_at DESC NULLS LAST`, whereas bare DESC means NULLS FIRST. The source
and timestamp composite index uses the same NULLS LAST ordering.

The Rust port preserves the previous TS repository's bare DESC ordering;
this query/index mismatch also exists in that source. All current production
rows have a non-null posted_at, but the schema permits nulls.

## Acceptance

- Choose whether to preserve NULLS FIRST through a matching index or specify
  NULLS LAST explicitly; preserve intended chronological behavior for null dates.
- Add a versioned migration if changing indexes; do not modify applied migrations.
- Verify indexed plans and latency for ascending/descending order and filtered
  queries, and check behavior with null dates and existing deduplication.

## Notes

Release `577ecb5`; checks and plan recorded during the 2026-10-02 production
switch. This was recorded for follow-up rather than changing ordering semantics
as part of the deployment.


## Resolution — 2026-10-02

Added `0004_news_chronological_indexes.sql` with two btree indexes matching
`DESC NULLS FIRST`: `news_items_posted_at_order` and
`news_items_source_posted_order`. Backward scans match bare ASC / NULLS LAST.
The historical NULLS LAST indexes remain available, and query semantics did
not change. Registered timestamp `1790957842163`, SHA-256
`ede5123f9e74197b56384c4fdaeb95818cf6d6d74eccc32b492a45fdc7f13ed3`.

Applied to the live news database as one transaction under the same advisory
lock used by Rust boot migrations; the same journal stores the file hash and
timestamp. No service/image restart was needed for this database-only change.

Validation: four Rust pg tests pass on a fresh local Postgres and after an
upgrade from the previous four migrations; repeated SQL application skips the
already recorded migration. Tests cover forward/backward query plans, source
filters, and NULL date ordering. Production plans use index scans: the actual
list query took 3.389 ms, ascending listing 0.257 ms, and descending channel
listing 0.285 ms. `list_news(limit=1)` through the running Rust MCP took 28 ms
versus 25–28 s before. Application and MCP reports are in
`/root/backups/rust-mcp-577ecb5-caGhks6K/index-migration-0004.*` and
`smoke.after-index.jsonl`.
