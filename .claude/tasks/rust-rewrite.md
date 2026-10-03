# Rewrite in Rust — the MCP server first, then the agent

**Status:** in-progress (MCP done on `feat/rust-rewrite`, not yet deployed; agent next)
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
- [ ] Image builds (`docker compose build`) — not run in the porting
      session.
- [ ] Deployed on the droplet; one day of signals processed normally
      (Telegram replies, a NashDom bill, the 08:00/09:00 digests, dreaming).
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
