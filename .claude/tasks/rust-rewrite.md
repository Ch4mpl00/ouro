# Rewrite in Rust — the MCP server first, then the agent

**Status:** in-progress (MCP deployed on 2026-10-02 at `577ecb5`; day-long observation and agent port remain)
**Priority:** P2
**Area:** whole repo; `crates/mcp` done, `packages/agent` next
**Created:** 2026-10-02

## Context

The whole system moves to Rust. The MCP server went first: it talks to the
agent only over the MCP protocol, so the Rust `mcp` container replaces the
TS one without the agent noticing. `packages/mcp` is deleted; `crates/mcp`
is the server.

**One domain, one file** carries over: `scheduler.rs` holds storage, cron,
the poller and the tools; `memory.rs` the types, patch engine, projection,
store, indexer, service and tools; and so on. Each domain adds its tools as
a `#[tool_router(router = <x>_tools)]` block on `McpTools`; `toolsets.rs`
maps a toolset name to that router.

## Compatibility kept on purpose

So the switch is reversible by redeploying the previous image:

- **State moved out of sqlite** into a Postgres database `mcp_state` beside
  the news store (decided 2026-10-02). `import-sqlite-state` copies the
  live tables once, keeping ids; the `mcp-data` volume with the old file is
  kept, unmounted, for rollback. Consequence: rolling back to the TS server
  after the switch means it resumes from the sqlite state as of the switch
  (Telegram may replay up to 24 h of updates, Gmail may re-signal recent
  mail).
- **Postgres** — `pg.rs` re-implements drizzle's migrator: same journal
  table, same hashes, same `when` ordering. Verified both ways on a
  throwaway pgvector DB: the Rust migrator applies nothing after the real
  drizzle migrator, and a Rust-migrated DB `pg_dump`s to the same schema.
  Migrations now take an advisory lock (fixes the `mcp` / `mcp-tunnel`
  concurrent-boot race the TS server also had).
- **Userbot** — the session stays in gramjs `StringSession` format and is
  imported into grammers; no re-login. Confirmed against the real local
  session (dialog listing works).
- **Gmail** — tokens in `integration_account` exactly as googleapis stored
  them; refresh 5 min early, retry once on 401.
- **Wire shapes** — tool names, input field names, JSON result shapes,
  signal content, ISO timestamps (`…000Z`) and the env-context text are
  unchanged; `toolsets.rs` tests pin both the default and the tunnel
  surface. A handler failure is an `isError` result, as with the TS SDK.
- **Sessions** — newest-wins for the full instance, multi-session for a
  restricted one; an unknown session id answers 404 "Session not found",
  which the agent's reconnect logic already matches.

## Deliberate differences

- `fetch_url`'s SSRF guard runs in the HTTP client's DNS resolver, so every
  redirect hop and a DNS answer that changed after validation are checked
  too (the TS guard only checked the first URL).
- `fetch_article` goes through the same guarded client.
- rmcp validates the `Host` header: compose sets `MCP_ALLOWED_HOSTS` (`*` on
  the tunnel, whose forwarded Host isn't predictable).
- `.env` is read from the working directory only (dotenv/config semantics).

## Acceptance

- [x] Every toolset, poller and CLI ported; TS package removed.
- [x] Unit tests + Postgres integration tests (`TEST_DATABASE_URL`).
- [x] End-to-end HTTP smoke of the full surface against a copy of a real
      `tokens.db` and a test Postgres.
- [x] Release image built locally for linux/amd64 and deployed from a clean
      archive of `577ecb5`; binary SHA-256 values matched on the droplet.
- [x] Deployed on the droplet; MCP protocol checks passed on both endpoints,
      agent and tunnel reconnected, first HN/Habr/channel harvests succeeded.
- [ ] One day of signals processed normally (Telegram replies, a NashDom bill,
      the 08:00/09:00 digests, dreaming); Gmail currently requires OAuth renewal.
- [ ] Agent port: `packages/agent` → `crates/agent`.

## Deploy notes

- **Local dev**: the sqlite file moves —
  `mkdir -p crates/mcp/data && mv packages/mcp/data/tokens.db* crates/mcp/data/`.
- **Droplet**: a release build of the crate is heavy (hundreds of crates,
  aws-lc). If the droplet is small, build the image elsewhere or add swap
  before `docker compose up -d --build`.
- **First deploy** (switch from TS): back up the sqlite file and Postgres,
  stop `mcp` + `mcp-tunnel`, run
  `docker compose run --rm -v agent-helper_mcp-data:/legacy:ro mcp import-sqlite-state --sqlite /legacy/tokens.db`,
  then start the Rust services. Later deploys are plain `pnpm deploy`.
- **Rollback**: retag the saved TS image as `mcp-tools-image:latest`, check
  out the previous commit on the droplet, `docker compose up -d --no-build`.
  The Postgres journal and the userbot session are shared; state written to
  `mcp_state` after the switch is not visible to the TS server.
- `glass_pumpkin` is pinned to `2.0.0-rc0` in Cargo.lock: `grammers-crypto`
  0.10 does not compile against rc1. Don't `cargo update` it away.


## Production switch — 2026-10-02

- Droplet checkout: detached `577ecb5`; Rust replaced `mcp` and `mcp-tunnel`.
  The existing TS agent and codex containers continue to run.
- State imported into `mcp_state`. Verified counts before enabling pollers:
  2 integration accounts, 769 Telegram messages, 1 Telegram cursor, 1 Gmail
  watermark, 2 settings, 13 scheduled tasks, 635 signals; 0 pending signals.
- Backups and measurements: `/root/backups/rust-mcp-577ecb5-caGhks6K/`.
  Includes a fully read-verified Postgres dump, final tokens/agent SQLite
  snapshots, previous compose and commit, import report and MCP checks.
  Old images retained as `mcp-tools-image:ts-backup-<service>-577ecb5`.
- SQLite snapshot procedure: stop every process sharing the state volume,
  copy the DB and WAL to a writable staging directory, use SQLite's backup
  API, then set the snapshot to `PRAGMA journal_mode=DELETE`. Mount that
  portable snapshot read-only for `import-sqlite-state`; a WAL-mode file
  without writable sidecar files can fail to open on a read-only mount.
- The auxiliary, no-poller TS `bench-mcp` also mounted the retired SQLite
  volume; it was stopped during cutover and remains stopped.
- MCP checks: 42 tools including 5 Tavily gateway tools on the full endpoint;
  18 tools and no gateway on the restricted endpoint. State/news/memory reads
  passed, supervisor skill validation passed, tunnel initialized. No restarts.
- Early Docker memory samples (MiB): primary MCP 296.3 → 171.2,
  tunnel MCP 137.4 → 6.1; combined 433.7 → 177.3 (about 59% less).
  Postgres rose from 134.3 to about 300; agent after restart about 206 versus
  95.9 before. The final sample settled to primary 119.2, tunnel 5.7, combined
  124.9 MiB (about 71% below the original combined sample); Postgres 204.7.
  These are early resident-memory samples, not a controlled load comparison.
  Host available RAM rose from 576 to about 780 MiB.
- Gmail returns `invalid_grant`. Migrated credentials byte-match the original
  snapshot, and the original TS Google SDK also fails to refresh that token
  with `invalid_grant`: renew Google authorization. No credentials were printed.
- `list_news(limit=1)` took 25–28 seconds; resolved by migration 0004; see
  `done/news-list-index-order.md` (MCP response now 28 ms).
  Habr extraction also emits many html5ever namespace warnings.
