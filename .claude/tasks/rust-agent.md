# The agent in Rust — crates/agent

**Status:** in-progress — runtime library ported and tested; workflow / supervisor next
**Priority:** P2
**Area:** packages/agent → crates/agent (runtime only)
**Created:** 2026-10-14

## Context

The MCP server runs as Rust since 2026-10-14 (`crates/mcp`, see
`rust-rewrite.md`). The agent supervisor is next, for the same reason: the
droplet has 2 GB. Scope, decided with the user: the **runtime** — everything
the `agent` container runs — moves; the offline tooling (`judging/`,
`scripts/`, `eval/`) stays in TypeScript and keeps reading the same
`agent.db` and Langfuse traces, whose shapes the Rust agent reproduces.

Like the MCP port: the TS agent stays in the image as the rollback, its
tests are ported one to one, and a fix in one goes into the other while both
exist.

## Libraries

| TS | Used for | Rust |
|---|---|---|
| `openai` SDK (chat completions) | four OpenAI-compatible providers: OpenAI, DeepSeek, Gemini, OpenRouter | `reqwest` + serde types of our own. The providers send fields the SDK types don't model (DeepSeek `thinking` + `reasoning_content`, OpenRouter `reasoning`, the decisions API); `async-openai`'s typed requests would fight that. |
| `@modelcontextprotocol/sdk` client | MCP over Streamable HTTP (and stdio for dev) | `rmcp` client (`transport-streamable-http-client-reqwest`, `transport-child-process`) — the crate the MCP server already uses |
| `effect` 4 | retries with backoff, timeouts, bounded concurrency, cancellation | `tokio` (`time::timeout`, `select!`), `futures` (`stream::iter().buffered(n)` keeps order), `tokio_util::sync::CancellationToken` for `AbortSignal` |
| `zod` | validating LLM-provided tool args, the workflow DSL, MCP replies | `serde` + hand-written checks that produce the same `path: message` errors the model is shown |
| `zod-to-json-schema` | inspection-only JSON schema of the DSL | dropped — nothing in the runtime reads it |
| `better-sqlite3` + `drizzle-orm` | `agent.db` (memory KV, trace mirror, journal progress) | `rusqlite` (bundled), SQL by hand; a drizzle-compatible migrator reads the same `migrations/*.sql` + `_journal.json` and the same `__drizzle_migrations` table, so TS tooling and either agent run on one file |
| `drizzle-kit` | generating migrations | stays in TS: `pnpm db:generate:agent` keeps generating the SQL both agents apply |
| `@langfuse/otel`, `@langfuse/tracing`, `@opentelemetry/*` | traces to Langfuse | explicit OTLP/HTTP JSON exporter (`reqwest` + `serde_json`, no OTel dependency) to `${LANGFUSE_BASE_URL}/api/public/otel/v1/traces`, Basic auth, the same `langfuse.*` span attributes the JS SDK sets (`langfuse.observation.type/input/output/metadata.<k>/level/status_message/model.name/model.parameters/usage_details`, `langfuse.trace.name`, `session.id`, `langfuse.trace.tags`) |
| `dotenv` | `.env` | `dotenvy` |
| `fast-check` | property tests | `proptest` |
| Node `fetch` | Codex service client | `reqwest` |
| `Intl.DateTimeFormat` | local time in the context block | `chrono-tz` |

## Compatibility kept on purpose

- `agent.db`: same schema, same migrations, same JSON column contents
  (`traces.observations` in the canonical Langfuse read shape) — the TS
  judge, improver and scripts read what the Rust agent writes.
- Langfuse: same observation names, types, metadata keys
  (`judge_node`, `activity`, `tool`, `skill`, `agent_id`, …) and ids (the
  local mirror keys on the OTel trace / span ids).
- Prompts and tool definitions are copied verbatim (see
  `feedback_verbatim_protocol_prompts`): working-memory instructions,
  synthetic tool descriptions, compiler prompt pieces, recovery and note
  instructions.
- Strings the model sees that are built with `JSON.stringify` keep its
  formatting (`serde_json` with `preserve_order`; pretty = two spaces).
  Lengths that bound text (clip, budgets) count UTF-16 units like JS.

## Plan

1. Crate skeleton, providers, retry, generation, tracing (null, local
   recorder, tee, Langfuse), agent.db + migrator.
2. Skills, session context, tool results, synthetic tools, MCP client,
   Codex client, decisions, engine, agent loop.
3. Workflow: DSL + checks, variables, compiler, executor, runner.
4. Supervisor: module, Telegram context, poll loop, activity journal, main.
5. Tests: every runtime test file of `packages/agent` (not `judging/`,
   `eval/`) as a Rust twin, each mutation-checked.
6. Image + compose (`command:` switch, TS kept for rollback), deploy.


## Acceptance

- [x] Rust workspace member and injected runtime library (`crates/agent`).
- [x] OpenAI / DeepSeek / Gemini / OpenRouter providers, retry, deadlines,
      cancellation, usage and tracing interfaces.
- [x] SQLite migration bytes/journal match TS; concurrent boot, memory KV,
      trace mirror and judge read/filter contracts tested.
- [x] Skills overlay, working memory, synthetic tools, Codex adapter,
      typed decisions + filter, engine and ReAct loop.
- [x] HTTP MCP reconnect / cancellation and no replay after an ambiguous
      failure; checked against SDK HTTP and the actual TS HTTP test process.
- [ ] Workflow DSL, variables, compiler, executor, patch / replan, runner.
- [ ] Supervisor, Telegram context, signal polling, activity journal, main.
- [ ] Complete per-test TS parity matrix and mutation checks for all runtime
      tests. Current tests port contracts/cases; this is not full TS coverage.
- [ ] Image / compose switch with TS rollback, runtime smoke and deployment.
- [ ] Live Langfuse ingestion verified (currently tested on generated OTLP
      and the local mirror, without calling Langfuse).

## Continuation checkpoint — 2026-10-14 (Codex)

Resumed Claude session `fa4f10c4-586f-45bf-ac41-4423405ac925`, which stopped
at `mcp.rs` at 17:57 UTC on the session limit. Worktree remains
`.claude/worktrees/rust-agent`, branch `feat/rust-agent`, based on `4fc041f`.
No binary or Docker switch yet: the production supervisor is still TS.

Completed Claude's disconnected module wiring and the unfinished MCP SDK
constructors, then ported the engine / loop / synthetic tools as one domain
in `src/agent.rs`, Codex and decisions. Synthetic tool definitions in
`src/synthetic_tools.json` are serialized from the TS registry (verbatim
prompts / JSON schemas); update that snapshot when TS definitions change.
All long-lived clients / stores are injected; loop registrations hold weak
engine references, and shutdown cancels loops, joins them, then closes clients.

Important differences/fixes discovered during the port:

- Session recovery belongs to our MCP client, not rmcp's automatic re-init:
  concurrent callers share one replacement. Only the typed `SessionExpired`
  transport failure proves a call never reached a tool. A matching string
  inside a 5xx body / tool failure cannot justify replay. Closing MCP awaits
  the service's transport finalizer and prevents later reconnects.
- `packages/mcp/src/http-transport.ts` now answers **404** for an unknown
  attached session id, matching Rust / Streamable HTTP; absent session id
  remains 400. The TS agent already recognizes `Session not found`. Added
  raw-wire tests, and checked the Rust client against the actual TS test
  process across eviction, a SIGKILL during a call and restart in both modes.
- A generation deadline cancels the operation's own token (including retry
  backoff / the actual provider) without cancelling its parent session.
- Environment gathering propagates SQLite read errors; an unreadable
  watermark is not silently treated as a first run. Invalid zones fail as
  `Intl.DateTimeFormat` does instead of rendering UTC under a wrong label.

### Current test mapping

| TS tests / contract | Rust |
|---|---|
| `providers/providers.test.ts`, `providers/retry.test.ts` | `providers.rs`, `retry.rs`, `provider_http.rs` |
| `session-context.test.ts`, `tool-results.test.ts` | `session_context.rs`, `tool_results.rs` |
| `tracing/tee.test.ts`, canonical trace mirror | `tracing.rs` |
| `test/agent-db.test.ts` | `agent_db.rs` (KV property sequences, trace/judge filters, migration bytes, concurrent boot) |
| `mcp-client.test.ts`, `test/mcp-reconnect.test.ts` | `mcp_client.rs`, `mcp_typescript.rs` (cross-language cases, randomized process histories still pending) |
| `agent-loop.test.ts`, `test/agent-loop.test.ts`, `test/write-skill.test.ts` | `agent_loop.rs`, `loop_properties.rs`, `skills.rs` (not yet every source test case) |
| `decisions.test.ts`, `test/decisions-filter.test.ts` | `decisions.rs` (builders, validation, usage, batching/order/threshold; remaining cases pending) |

Validation commands from the worktree:

```sh
cargo test -p agent --offline
cargo test -p agent --offline --test mcp_typescript -- --ignored
cargo clippy -p agent --all-targets --offline -- -D warnings
cargo fmt -p agent -- --check
pnpm exec tsc --noEmit
pnpm exec vitest run packages/mcp/src/http-transport.test.ts packages/mcp/test/http-transport.test.ts packages/agent/src/mcp-client.test.ts
```

The cross-language tests are ignored by default because they need Node / pnpm
dependencies. They launch only `packages/mcp/test/support/mcp-child.ts`, whose
sole tool writes a temporary ledger; they never start production pollers.
Local node_modules directories link dependencies from the main checkout inside
the ignored directories; no package lifecycle/build scripts were run. On a new
checkout, install pnpm dependencies before the TypeScript / cross-language checks.

Seven compiling mutations were caught: skill whitelist intersection changed
to union, workers allowed to delegate, child-created skill-write authorization,
unordered tool replies, deadline leaving provider cancellation live, every
result labeled text, and replay based on text inside an ambiguous HTTP failure.
Sources restored after each mutation. This is a targeted check, not a complete
mutation audit of the TS runtime test suite.

**Next unit:** port `workflow.ts` as one domain (DSL / validation → variable
resolution → compiler → executor → patch/replan → runner), copying its TS
contracts/tests before wiring the supervisor. Keep the production entry point
and compose command on TS until those acceptance items are satisfied.


### Verified result at this checkpoint

- `cargo test -p agent --offline -- --include-ignored`: **52 passed**, including
  both cross-language process tests; no production integrations were called.
- `pnpm exec vitest run …` for the two HTTP transport suites and agent MCP
  client suite: **23 passed** (3 files).
- `cargo clippy -p agent --all-targets --offline -- -D warnings`,
  `cargo fmt -p agent -- --check`, `pnpm exec tsc --noEmit`, `git diff --check`:
  passed.
- The TS transport fix is isolated in local commit `b686e49`.
