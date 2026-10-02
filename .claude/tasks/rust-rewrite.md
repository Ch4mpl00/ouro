# Rewrite in Rust — packages/mcp first

**Status:** in-progress
**Priority:** P2
**Area:** whole repo; currently `crates/mcp`
**Created:** 2026-10-02

## Context

The whole system moves to Rust. `packages/mcp` goes first: it talks to the
agent only over the MCP protocol, so a Rust `mcp` container can replace the
TS one without the agent noticing. `packages/agent` follows once the server
side is at parity.

The Rust port lives beside the TS one in a Cargo workspace (`Cargo.toml` at
the root, `crates/mcp`) until the swap. It opens the **same**
`packages/mcp/data/tokens.db` and will open the same Postgres — no data
migration, so the swap is reversible by redeploying the TS image.

**One domain, one file** carries over: `scheduler.rs` holds storage, cron,
the poller and the tools; `signals.rs` the queue and its tools; and so on.
Each domain adds its tools as a `#[tool_router(router = <x>_tools)]` block on
`McpTools`; `toolsets.rs` maps a toolset name to that router.

A toolset whose domain is not ported yet is **refused at boot**, not skipped.
That is why the default (unset `MCP_TOOLSETS`) surface does not start yet.

## Ported

- [x] Skeleton: `main.rs` (composition root, shutdown), `server.rs` (handler,
      stdio + Streamable HTTP, "newest wins" sessions), `toolsets.rs`
- [x] `db.rs` — sqlite schema, additive migrations, system-task seed
- [x] `settings.rs` — settings KV, timezone
- [x] `signals.rs` — `signals` (get_next_signal), `dreaming` (list_signals)
- [x] `scheduler.rs` — poller + schedule/list/cancel + get/set timezone
- [ ] `telegram.rs` — config only so far; Bot API client, chat log,
      long-poll poller, typing/status, `telegram` + `telegram-send` tools
- [ ] `gmail.rs` — OAuth (`gmail:auth` CLI), poller, attachments
- [ ] `monobank.rs`
- [ ] `pdf.rs`, `fs.rs`, `fetch.rs` (SSRF guard)
- [ ] Postgres layer (`sqlx` + `pgvector`) — needed by news, knowledge, memory.
      Drizzle migrations: keep applying the existing SQL files in order and
      read drizzle's `__drizzle_migrations` journal, so a DB migrated by TS is
      not re-migrated
- [ ] `embeddings.rs`, `news.rs` (HN, Habr, channel posts, article extract)
- [ ] `knowledge.rs`, `memory.rs` (port `service.ts` with its tests first —
      the whole contract is tested against an in-memory store)
- [ ] `skills.rs` — keep `appendPatch` byte-identical with the agent's copy
- [ ] `userbot.rs` — MTProto via `grammers`
- [ ] `gateway.rs` — third-party MCP upstreams via rmcp's client
- [ ] CLI entry points (`gmail:auth`, `userbot:auth`, `embed:backfill`, …) as
      `[[bin]]` targets
- [ ] Dockerfile stage + compose swap for `mcp` and `mcp-tunnel`

## Acceptance

- `crates/mcp` serves the full default surface and the tunnel surface;
  `toolsets.rs` tests pin both lists, matching `toolsets.test.ts`.
- Running against a copy of the prod `tokens.db` + Postgres dump: no schema
  change, all pollers emit the same signal content (the agent's skills parse
  scheduler headers literally).
- Compose `mcp` and `mcp-tunnel` run the Rust binary; the TS package is
  deleted.

## Notes

- **Host allowlist.** rmcp rejects any `Host` that is not loopback by
  default. Compose must set `MCP_ALLOWED_HOSTS=mcp,mcp:3000` (and
  `mcp-tunnel,mcp-tunnel:3001` on the tunnel) or the agent and tunnel-client
  get 403.
- **Cron semantics.** `croner` defaults match `cron-parser`: day-of-month OR
  day-of-week, optional seconds field. Pinned by a test.
- **Timestamps** handed to the agent keep JS `toISOString()` format
  (`…T09:00:00.000Z`).
- **Userbot risk.** `grammers` is younger than gramjs; the gramjs session
  string won't carry over, so expect one `userbot:auth` re-login.
- **Langfuse** has no Rust SDK — relevant for the agent port, not this one.
- The 2026-07-28 MCP protocol is sessionless; rmcp serves it statelessly,
  so "newest wins" only applies to legacy-protocol clients (the current
  agent).
