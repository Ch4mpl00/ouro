# mcp-tools

A personal-agent system structured as two independent processes in one pnpm workspace:

- **`packages/mcp`** — stateless MCP server. Wraps Gmail / Telegram / Monobank
  as primitive tools and runs the pollers that turn external events into
  signals on a queue. Knows nothing about the agent.
- **`packages/agent`** — agent supervisor. Pulls one signal at a time from
  MCP, loads the matching skill, runs one DeepSeek session, and stops.
  Knows nothing about MCP internals — only the actions exposed by the
  MCP protocol.

Both communicate strictly through the MCP protocol. Neither imports code from
the other. Deployed as two containers (`docker-compose.yml`).

**The MCP server exists twice.** `crates/mcp` is a Rust port of
`packages/mcp` (the droplet is 2 GB; the Rust server is about half the
memory). Compose runs the Rust one (`command: mcp-tools`); the TS one stays
in the image as the rollback (`command: pnpm mcp:serve`). They share the
databases, the migrations (`crates/mcp/migrations` must stay byte-identical
to `packages/mcp/src/db/pg/migrations`, a test checks), the `.feature`
specs and the property tests, ported one to one (`crates/mcp/tests/*.rs`
mirror `packages/mcp/test/*.test.ts`). **A fix in one server goes into the
other**, with its test, until the TS server is retired. Status and history:
`.claude/tasks/rust-rewrite.md`.


**The agent runtime also exists twice.** `crates/agent` contains the Rust
runtime; compose selects `agent-supervisor`. The TypeScript runtime in
`packages/agent` remains available with
`command: sh -c "pnpm setup:agent && pnpm agent:start"`. Both use the same
`agent.db` migrations and trace shapes. Offline judging, improvement, GAIA and
CLI scripts stay in TypeScript. The runtime tests are ported one to one
(`crates/agent/tests/*.rs` mirror the runtime tests of `packages/agent`, one
Rust test per TS case, the fast-check properties as proptest). Fix shared
runtime contracts in both ports, with their tests; run `cargo test -p agent`
and `pnpm typecheck`. The Rust binary handles SQLite migrations itself. Port
status and rollout evidence: `.claude/tasks/rust-agent.md`.

## How a signal turns into action

1. A poller inside `packages/mcp` fires on its own cadence — Gmail (1 min),
   Telegram bot getUpdates (long-poll), userbot channels (30 min),
   scheduler (30s, fires cron rows from `scheduled_tasks`).
2. When it sees something new, it calls `signals.record({ source, content })`,
   which inserts a row into the `signals` queue in the `mcp_state` Postgres
   database.
3. The supervisor (`packages/agent/src/supervisor/main.ts`) loops on
   `get_next_signal`. Routing is by source, decided in `supervisor/module.ts`:
   `scheduler` runs through the `workflow/` module (compile → execute), every
   other source runs the primary AgentLoop. A compile failure degrades to the
   AgentLoop in the same trace; an execute failure goes to `recovery` instead
   of being retried. The AgentLoop path loads:
   - `skills/orchestrator.md` + `skills/routing.md` — always; how to use
     working memory and when to delegate to a domain skill.
   - `skills/<signal.source>.md` (with `skills.default/` fallback) — only for
     transport sources (`telegram`, `scheduler`); domain work is delegated to
     a sub-agent with that source's skill.
4. Plus a session-context block (local time, tz, watermarks) and the signal's
   `envContext` (per-source env addendum, e.g. default Telegram chat id).
   For `telegram` signals the supervisor also preloads recent chat/topic
   history (`supervisor/telegram-context.ts`) before the first LLM turn.
5. The signal's `content` is pushed as the first user message. The loop runs;
   every side effect (Telegram reply, DB write) is a tool call.

To add a new domain: drop a `skills.default/<name>.md` + emit signals with
`source=<name>` from a new poller. No supervisor change.

## Layout

```
mcp-tools/
├── pnpm-workspace.yaml
├── package.json            workspace orchestration scripts only
├── tsconfig.json           shared TS config (covers both packages)
├── .mcp.json               registers packages/mcp with Claude Code (stdio)
├── .env.mcp                MCP container env (integration creds)
├── .env.agent              agent container env (DeepSeek key, model)
├── .env.example, .env.mcp.example, .env.agent.example
├── docker-compose.yml      mcp + agent, plus mcp-tunnel + tunnel-client
├── Dockerfile              one image for both
├── skills.default/         shipped skills (git-tracked, read-only fallback)
├── skills/                 live overlay, gitignored; dreaming writes here
├── storage/                downloaded Gmail attachments (gitignored)
└── packages/
    ├── mcp/
    │   └── src/
    │       ├── server.ts                    composition root: deps, pollers, HTTP/stdio transport
    │       ├── mcp-server.ts                createServer(deps): one MCP endpoint
    │       ├── db/state.ts                  the `mcp_state` database (create, migrate, seed)
    │       ├── toolsets.ts                  named tool groups (MCP_TOOLSETS scoping)
    │       ├── gateway.config.json          third-party MCP upstreams (git-tracked, secret-free)
    │       ├── tools/                       MCP-exposed actions
    │       └── services/{gmail,telegram,monobank,scheduler,news,memory,pdf,signals,settings,gateway}
    └── agent/
        ├── data/agent.db                    agent-side state (memory KV + trace mirror)
        └── src/
            ├── db/{client,memory,trace-store,schema}.ts + migrations/  Drizzle (sqlite)
            ├── supervisor/{main,loop,module,telegram-context}.ts  poll loop,
            │                                     per-signal routing, tg history
            ├── workflow/                      dynamic-workflow module (compile + execute)
            │   ├── index.ts                   createWorkflowRunner facade (runForSignal)
            │   ├── compile.ts                 signal → validated Workflow (LLM)
            │   ├── execute.ts                 runtime that walks the steps
            │   ├── dsl.ts                     Workflow step schema + parse
            │   └── variables.ts               ${path} substitution + variable store
            ├── engine.ts, agent-loop.ts     LLM runner (ReAct loop)
            ├── activity-journal.ts          traces → ouro/activity journal notes (background
            │                                 loop inside the supervisor)
            ├── providers/                   ChatProviders: openai, gemini, deepseek,
            │                                 openrouter (opt-in, "openrouter/…" models)
            ├── decisions.ts                 typed classification (Jev via OpenRouter's
            │                                 decisions API) + presets (filter); the
            │                                 workflow `decide` step runs them
            ├── synthetic-tools.ts           agent-side tools (set_memory, …)
            ├── mcp-client.ts                StreamableHTTP client
            ├── session-context.ts           markdown context block builder
            ├── skills.ts                    two-layer loader (live → default)
            ├── tracing/{index,langfuse}.ts  Tracer interface + Langfuse adapter
            └── db/{client.ts, memory.ts}    KV helpers
```

## Stack

- TypeScript (ESM, `module: Preserve`, `moduleResolution: Bundler`)
- Effect 4 (`effect@4.0.0-rc.113`, pinned) for asynchronous orchestration
- `@modelcontextprotocol/sdk` (stdio + StreamableHTTP transport)
- `better-sqlite3` for Node code; `sqlite3` CLI from Bash for ad-hoc queries
- `googleapis` + `google-auth-library` (Gmail)
- `telegram` (gramjs / MTProto) for userbot channel reading
- `cron-parser` v5 for scheduled tasks
- `openai` SDK pointed at DeepSeek / Gemini / OpenRouter (OpenAI-compatible)

## Three databases — split by ownership

- **`mcp_state`** (Postgres database next to the news store;
  `packages/mcp/src/db/state.ts`) — MCP's private state: OAuth tokens and
  the userbot session, Gmail watermarks, the Telegram cursor and chat log,
  the `signals` queue, `scheduled_tasks`, settings. The server derives it
  from `DATABASE_URL` (same server, database name `mcp_state`;
  `STATE_DATABASE_URL` overrides) and creates + migrates it on boot. Its
  schema, migration table and advisory-lock key are the Rust server's
  (`feat/rust-rewrite`), so either server runs on a database the other
  created. It replaced the sqlite `tokens.db`. Don't read or write it from
  agent code — go through MCP tools.
- **`packages/agent/data/agent.db`** (sqlite) — agent's domain state:
  `memory` (freeform KV, e.g. `news_digest.last_read_at`) plus the local
  trace mirror (`traces` + per-node `judgements`). Schema lives in code at
  `packages/agent/src/db/schema.ts` (Drizzle ORM, sqlite); migrations are
  generated with `pnpm db:generate:agent` and applied on boot
  (`db/client.ts`, also `pnpm setup:agent`).
- **Postgres + pgvector** (containerized, `postgres` service in
  docker-compose) — the news / RAG store **and unified memory**. One table
  `news_items` unifies HN/Habr articles and harvested Telegram channel posts;
  rows have a 1536-dim `embedding` column (text-embedding-3-small). Unified
  memory adds `memory_projects` / `memory_project_docs` / `memory_doc_patches`
  / `memory_facts` (the read models) plus `memory_index` (the search
  projection — the only memory table with vectors). Owned by MCP; the agent
  reaches it only through MCP tools (`search_news`, `recall`, `read_doc`, …).
  Schema lives in code at `packages/mcp/src/db/pg/schema.ts` (Drizzle ORM);
  migrations are generated with `pnpm db:generate:pg` and applied on server
  boot under an advisory lock (`mcp` and `mcp-tunnel` boot at once). The
  Rust server applies the same files with the same journal, so an applied
  migration file must never change.

Schemas: `packages/mcp/src/db/state.ts` (`mcp_state`, versioned migrations
inline), `packages/agent/src/db/schema.ts` (agent sqlite, Drizzle),
`packages/mcp/src/db/pg/schema.ts` (PG, Drizzle). Everything applies
automatically on boot; `pnpm db:init` does it without starting the servers.

For ad-hoc queries during development:

```bash
sqlite3 -json packages/agent/data/agent.db "SELECT * FROM memory"
sqlite3      packages/agent/data/agent.db "UPDATE memory SET value=? WHERE key=?"
docker compose exec postgres psql -U mcp -d mcp_state -c "SELECT * FROM signals ORDER BY id DESC LIMIT 5"
```

For multi-line / quote-heavy SQL, use a heredoc. Always single-quote
string literals; double single quotes inside (`'O''Brien'`).

## Code structure: modules + DI

Domain code is organised as **modules** with explicit **dependency
injection**. Every long-lived piece of state (DB pool, OpenAI client,
EmbeddingService, storage layer) is built once in the composition root
(`server.ts main()` or a script's `main()`) and passed down. No
`getX()` singletons, no service locators, no global handles in
business code.

### Rules

1. **Factory functions, not classes.** State that needs scoping lives
   in a closure built by `createX(deps)`. Return a plain object with
   the methods consumers need. No `this`, no `new`.

2. **Every module has a `module.ts`.** It declares a `XxxModule`
   interface (the public surface) and a `createXxxModule(deps): XxxModule`
   factory that wires everything inside. See
   `services/news/module.ts` as the canonical example. Pattern:

   ```ts
   export interface XxxModule { /* exposed services */ }
   export interface XxxModuleDeps { /* required deps from outside */ }
   export function createXxxModule(deps: XxxModuleDeps): XxxModule { … }
   ```

3. **Generic infrastructure is dependency-free.** A reusable layer
   (e.g. `services/embeddings/`) declares interfaces (`EmbeddingRepository`,
   `EmbeddingProvider`, `Chunker`) and a `createEmbeddingsModule({repo, …})`
   factory that takes them. It never imports a concrete table or
   domain type. Domain modules supply the implementations.

4. **Repos live with the table they own.** Implementation of
   `EmbeddingRepository` for `news_items` lives in
   `news/embedding-repository.ts`, not in `embeddings/`. The interface
   is in the generic module; the impl is in the domain.

5. **DB handle is injected.** `db/pg/client.ts` exports
   `createPgClient(): { db, ensureReady, close }` only. Anything that
   talks to PG accepts `db: Database` (factory parameter) or sits
   behind a storage/repo factory that does.

6. **Tools and pollers take their deps in the signature.**
   `registerXxxTools(server, deps)`, `startXxxPoller(deps)`. They
   never reach for a global handle inside the handler.

7. **Composition root is the only place that knows the full graph.**
   `server.ts main()` calls the factories in order, threads the
   result through `createServer({...})` and `startXxxPoller({...})`.
   Scripts do the same for their narrower scope and `await pg.close()`
   in `finally`.

8. **Add a new domain → add a new module.** Create
   `services/<domain>/module.ts` with `createXxxModule({db, ...})`,
   instantiate it in `server.ts main()`, pass to whoever needs it. Do
   not extend `NewsModule` with unrelated concerns just because PG is
   already there.

9. **Use Effect for asynchronous orchestration.** Parallel tasks,
   concurrency limits, cancellation, timeouts, retries/backoff and resource
   cleanup should use Effect 4. Keep child tasks within their parent's
   lifetime, propagate `AbortSignal` to SDK/HTTP calls and await finalizers
   before closing traces or clients. Use `Effect.forEach` / `Effect.all`
   with an explicit concurrency limit; convert to Promises at public API
   boundaries. Keep the factory + DI structure above.

   Reuse `packages/agent/src/generation.ts` for LLM generation/tracing and
   `providers/retry.ts` for transient failures. Keep automatic retries in
   one layer (SDK retries are disabled), and do not automatically replay
   tool side effects. See `agent-loop.ts` and `workflow/execute.ts` for
   existing patterns. Simple sequential SDK adapters can remain
   `async`/`await`; add Effect when introducing orchestration.

### Anti-patterns (don't)

- `getPgDb()` / `getXxxModule()` exported helpers that lazy-init a
  singleton. They look convenient but turn every consumer into a
  hidden coupling on global mutable state and make tests painful.
- `class XxxService` with only a constructor and one method — that's
  a factory function in disguise.
- Importing a concrete table or schema from `services/<generic>/`.
  Move the interface up, the impl down.
- Handlers that pull deps inside their body (`const { news } =
  getModule()`). The handler's signature must be the contract.

## Task tracking

Planned work and tech debt live under `.claude/tasks/` — one markdown
file per task with frontmatter-style fields (`Status`, `Priority`,
`Area`, `Created`). Format is documented in `.claude/tasks/README.md`.

When picking work, scan that directory first — there's usually a
written-up task with context, rather than starting fresh from a half-
remembered Slack thread. When agreeing on new tech debt during a
design discussion, add a file there before moving on, so the decision
doesn't evaporate.

## MCP tools (signal-emitting + agent-callable)

Defined in `packages/mcp/src/tools/`. The agent calls these via MCP; you
also see them when running `claude` locally with `.mcp.json` registered.

- **Gmail** — `list_nashdom_mails`, `download_gmail_attachment`
- **Telegram bot** — `send_telegram_message`, `edit_telegram_message`,
  `send_telegram_chat_action`, `get_telegram_chat_history`
- **Telegram userbot (read-only MTProto)** — `list_userbot_dialogs`,
  `list_channel_posts`
- **Monobank** — `list_monobank_transactions` (no poller; reactive only)
- **News** — `list_news`, `fetch_article` (HN, Habr — both
  upsert into news_items and embed inline), `search_news` (semantic
  search across the unified store: HN, Habr, channel posts)
- **PDF** — `read_pdf`
- **Files** — `read_file`
- **Skills** — `list_skills`, `read_skill` (exact active `.md` filenames +
  contents, plus the improver's `.patch.md` overlay and the composed
  `effectiveInstructions` the agent actually runs)
- **Unified memory** — shared by every agent, see the section below.
  `recall`, `remember`, `get_fact`, `update_fact`, `list_memory`,
  `create_project`, `read_doc`, `append_doc`, `patch_doc`, `write_doc`,
  `doc_history`, `revert_patch`, `journal_append`, `journal_tail`
- **Signals queue** — `get_next_signal`, `list_signals`
- **Scheduler** — `schedule_task`, `list_scheduled_tasks`,
  `cancel_scheduled_task`
- **Env** — `get_timezone`, `set_timezone`
- **Third-party (via gateway)** — when `gateway.config.json` lists upstreams,
  `services/gateway/` makes own-MCP an MCP *client* to them too and re-exposes
  their tools namespaced as `${prefix}__${tool}` (e.g. `tavily__tavily_search`).
  The agent still sees one merged endpoint. Onboarding (config + secret + skill
  frontmatter, no code) is in `.claude/tasks/mcp-gateway.md`.

### Tool scoping (`MCP_TOOLSETS`)

One MCP process serves one audience. `packages/mcp/src/toolsets.ts` holds the
named toolset → registrar map (`gmail`, `telegram`, `telegram-send`,
`monobank`, `pdf`, `fs`, `signals`, `news-read`, `knowledge`, `dreaming`,
`userbot`, `scheduler`, `skills`, `memory`);
`MCP_TOOLSETS=news-read,telegram-send,skills` makes an instance register only
those groups. Unset/empty = every group, i.e. the behaviour before scoping
existed. An unknown name is a boot error.

A **restricted instance is also multi-session** (`runHttpTransport`'s
`multiSession`), which is what lets several agents hold a memory connection at
once: an instance scoped to `memory` has no signals tools, so nothing races
for signal delivery.

Scoping works by **not registering**, so an out-of-scope tool never appears in
`tools/list` — invisible, not merely rejected. A restricted instance also skips
the gateway entirely, because upstream tools are namespaced at runtime
(`tavily__*`) and can't be expressed in the allow-list. Rationale and the wider
auth design: `.claude/tasks/mcp-auth-and-tool-scoping.md`.

## Unified memory

One memory every agent shares — the droplet supervisor, Claude Code sessions,
anything on the far side of the ChatGPT tunnel. Lives in
`packages/mcp/src/services/memory/`; design and rationale in
`.claude/tasks/unified-memory.md`.

Three read models, one search projection:

- **Projects** are folders of markdown documents (`passport.md`, `roadmap.md`,
  `progress.md`, …), not a fixed schema. The agent reads and patches each
  document individually, which is the interface LLMs are best at.
- **Facts** are flat freeform statements — what `knowledge_base_notes` held,
  plus a lifecycle state and an update path.
- **The journal** is the third read model: operational records a workflow
  appends and reads back by recency (`journal_tail`), e.g. the `news-digest`
  stream the digest dedups against. Streams are named categories
  (`digest/news`) with a stored capacity (default 100, oldest dropped
  first). Entries are immutable, never indexed and keep no patch history —
  records, not knowledge. Invariants: `services/memory/journal*.test.ts`
  (fast-check against a reference model; the Postgres race tests need
  `TEST_DATABASE_URL`).
- **`memory_index`** is the only table with embeddings. Documents and facts are
  chunked into it with their subject denormalised into the indexed text, so a
  fragment matches even though the stored document stays clean.

The agent's flow is two-step, the same search→read shape that fixed the
planner's snippet failures on GAIA: `recall` returns **refs**
(`doc:leetcode-graphs/roadmap.md#2`, `fact:88`), then `read_doc` / `get_fact`
loads the whole thing.

Rules worth knowing before touching the code:

- **Writes are search/replace on quoted literals, never line numbers.** An
  off-by-one line number corrupts silently; a quote that doesn't match fails
  loudly and changes nothing. Each `old` must be unique, all edits apply or
  none do, and a miss comes back with the literal from the document that
  nearly matched (the model normalised an em-dash, a «quote», ё→е).
- **`append_doc` is the safe default** — it cannot destroy text and needs no
  version. `patch_doc` needs `expected_version`. `write_doc` is the only op
  that can lose content.
- **Every write is versioned with a compare-and-swap**, which is the whole
  concurrency story for several agents on one document.
- **Every write records a patch** with the previous body, an actor and the
  rationale (the utterance that caused it). `revert_patch` undoes one; an
  older patch whose text later writes touched fails cleanly and names the
  version a `rollback: true` would restore. History is never rewritten.
- **Indexing failure never fails a write.** Documents read and patch with the
  embedder down; `pnpm embed:backfill` drains the NULL-vector backlog.

The rules live in `service.ts` and the store underneath is dumb CRUD, so the
whole contract is tested against `store.memory.ts` without a Postgres.

**Who writes:** `MCP_MEMORY_ACTOR` names the instance (`supervisor` for `mcp`,
`chatgpt` for `mcp-tunnel`) and is stamped onto every patch. Audit metadata,
never access control — one shared space (D6). It is a property of the
instance, not something a client declares, because a self-declared actor is
forgeable; real per-token identity is `.claude/tasks/mcp-auth-and-tool-scoping.md` A2.

## Agent skills

Live under `skills.default/` (git-tracked, shipped in image) with an
optional live overlay in `skills/` (gitignored, mounted as a Docker
volume — written by the `dreaming` skill when it self-revises).

`readSkill(name)` reads `skills/<name>.md` first, falls back to
`skills.default/<name>.md`. Naming is signal-source-based:

- `nashdom-bill`, `news-digest`, `tech-digest`, `dreaming`, `scheduler`,
  `telegram` — primary domain skills, loaded per signal.source.
- `routing`, `orchestrator`, `worker` — always loaded on the AgentLoop path
  (`worker` is the default skill for a sub-agent with no domain skill).
- `planner` — the workflow compiler's system prompt (scheduler signals).
- `recovery` — spawned on a failed signal to phrase the failure to the user.

The improver writes an append-only overlay at `skills/<name>.patch.md`; the
runtime glues it onto the end of the body with `appendPatch`, so the
instructions in force are body + patch. **`packages/mcp/src/services/skills/`
re-implements that composition** for the `read_skill` export — the two
packages may not import each other, so if you change `appendPatch` (marker,
spacing), change the MCP copy too or the export starts describing a skill
nobody runs. `module.test.ts` pins the expected output as a literal.

## Running

- `pnpm db:init` — create/migrate `mcp_state` and the PG store + apply
  agent/agent.db (all also happen on boot).
- `pnpm mcp:serve` — start the MCP server. **Do not run locally if the
  droplet is also running it** — Telegram getUpdates is exclusive and the
  second poller causes 409 Conflict.
- `pnpm agent:start` — start the supervisor (long-running loop).
- `pnpm gmail:auth` — one-time OAuth bootstrap. Writes to `mcp_state`
  (needs `DATABASE_URL`).
- `pnpm gmail:list-unread` — debug helper.
- `pnpm telegram:get-chat-id` — discover your chat id (after sending any
  message to your bot).
- `pnpm userbot:auth` — one-time MTProto login (phone + code).
- `pnpm typecheck` — typecheck both packages.
- `cargo test -p mcp-tools` — the Rust server's tests (unit, property,
  Postgres via an auto-started pgvector testcontainer unless
  `TEST_DATABASE_URL` is set, the shared specs, the reference model);
  `cargo clippy --all-targets`, `cargo fmt`.
- `pnpm spec` — the executable specification: `packages/mcp/specs/*.feature`,
  plain-English scenarios run by cucumber-js against the real server over MCP
  with a Testcontainers Postgres (needs Docker; `TEST_DATABASE_URL` uses a
  throwaway server instead). **The `.feature` files are the user's**: never
  edit one to make a failing run pass — fix the code, or ask. New behaviour
  gets a scenario there first. They are byte-identical to the Rust port's.
  See `packages/mcp/specs/README.md`.
- `pnpm spec:model` — the reference model of memory documents:
  `packages/mcp/model/model.ts` says what write/append/patch/revert mean
  (~200 lines, no database) and `properties.test.ts` checks its properties
  with fast-check (part of `pnpm test`). `server.check.ts` sends random call
  sequences, then overlapping calls from several sessions, to both the model
  and the real server; answers must match and every concurrent history must
  be linearizable. Needs Docker (or `TEST_DATABASE_URL`); `MODEL_CASES`
  raises the case count. **Same ownership as the `.feature` files**: when the
  server and the model disagree, fix the code or ask — a rule change starts
  in `model.ts`, and its properties must still pass. See
  `packages/mcp/model/README.md`.
- `pnpm test` — vitest. With `TEST_DATABASE_URL=postgres://…` the Postgres
  tests run too (point it at a throwaway pgvector server, never prod).
- `pnpm db:generate:pg` — regenerate Drizzle migrations after editing
  `packages/mcp/src/db/pg/schema.ts`. The new `*.sql` file lands in
  `packages/mcp/src/db/pg/migrations/` and is applied on next mcp boot.
- `pnpm db:generate:agent` — same for the agent sqlite schema
  (`packages/agent/src/db/schema.ts` → `packages/agent/src/db/migrations/`,
  applied on agent/judge-worker boot via `db/client.ts`).
- `pnpm db:migrate:channel-posts` — one-shot copy of the legacy sqlite
  `channel_posts` table (the old `tokens.db`, `MCP_DB_PATH`) into PG
  `news_items` + inline-embed. Idempotent.
- `pnpm memory:import-notes` — one-shot copy of `knowledge_base_notes` into
  memory facts (idempotent; the source table is left untouched).
- `pnpm embed:backfill` — re-attempt embeddings for any `news_items`
  rows where `embedding IS NULL` (typically left behind by an OpenAI
  outage during inline embed).

Deploy: `scripts/deploy.sh [services…]` from the laptop — it builds the
linux/amd64 image locally (the droplet can't compile the Rust crate), ships
it over SSH and runs `docker compose up -d --no-build` on a checkout of the
same commit, which must be pushed. Named volumes (`mcp-data`, `mcp-storage`, `agent-data`,
`agent-skills`, `pg-data`) persist state across rebuilds. First boot
needs `.env.postgres` (POSTGRES_USER / PASSWORD / DB) and
`OPENAI_API_KEY` in `.env.mcp`.

The two evaluation services — `judge-worker` (per-node Codex judge) and
`improve-worker` (closed-loop improver) — sit behind the `evals` compose
profile and are OFF since 2026-08-31: scoring live prod runs wasn't worth
the codex quota, and the plan is golden sets instead. A plain
`docker compose up -d --build` skips them; bring them back with
`docker compose --profile evals up -d judge-worker improve-worker`. The
`codex` service itself stays ON regardless — the agent's `code_agent` tool
runs through it.

### ChatGPT connector (Secure MCP Tunnel)

Two extra compose services, both optional — the rest of the stack runs
without them:

- **`mcp-tunnel`** — a second `mcp` process on port 3001 with
  `MCP_NO_POLLERS=1` and
  `MCP_TOOLSETS=news-read,telegram-send,skills,memory`: news reading, one
  Telegram write, the skills export, unified memory, and no gateway. The exact
  list is pinned by a test in `toolsets.test.ts` — if that list and this env
  var drift apart, ChatGPT silently gets a surface nobody chose. `expose`
  only, like `mcp` — never `ports`. It shares the `mcp-data` volume because
  `send_telegram_message` appends to the `telegram_messages` chat log, and the
  same Postgres as `mcp`, so **memory really is shared**: ChatGPT reads and
  writes the same projects and facts as the supervisor, signed
  `MCP_MEMORY_ACTOR=chatgpt` in `doc_history`.
- **`tunnel-client`** — `ghcr.io/openai/tunnel-client`, OpenAI's daemon. It
  long-polls `api.openai.com` outbound and forwards MCP requests to
  `http://mcp-tunnel:3001/mcp`. Nothing is published; ChatGPT only ever knows
  a `tunnel_id`. In ChatGPT the connector is a developer-mode app with
  *Connection → Tunnel*.

  Credentials live in `.env.mcp` as `OPENAI_TUNNEL_ID` +
  `OPENAI_TUNNEL_API_KEY`; the service's entrypoint is the single place that
  maps them onto the daemon's `CONTROL_PLANE_TUNNEL_ID` /
  `--control-plane.api-key`. Note that compose expands `${VAR}` from the
  shell and the project-root `.env`, **not** from a service's `env_file`, so
  the mapping has to happen inside the container. The key should be a
  restricted one (Tunnels Read + Use), never the admin key.
