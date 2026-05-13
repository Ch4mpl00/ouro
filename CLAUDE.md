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

## How a signal turns into action

1. A poller inside `packages/mcp` fires on its own cadence — Gmail (1 min),
   Telegram bot getUpdates (long-poll), userbot channels (30 min),
   scheduler (30s, fires cron rows from `scheduled_tasks`).
2. When it sees something new, it calls `recordSignal({ source, content,
   envContext })` which inserts a row into the `signals` queue in
   `packages/mcp/data/tokens.db`.
3. The supervisor (`packages/agent/src/supervisor.ts`) loops on
   `get_next_signal`. When a signal pops, it loads three things:
   - `skills/<signal.source>.md` (with `skills.default/` fallback) — the
     primary domain skill.
   - `skills/routing.md` — always loaded; tells the model to delegate to
     another skill if the prompt matches a different domain.
   - `skills/handoff.md` — always loaded; rules for escalating reasoning
     effort mid-session.
4. Plus a session-context block (local time, tz, watermarks) and the signal's
   `envContext` (per-source env addendum, e.g. default Telegram chat id).
5. The signal's `content` is pushed as the first user message. DeepSeek
   runs the session; every side effect (Telegram reply, DB write) is a
   tool call.

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
├── docker-compose.yml      two services: mcp + agent
├── Dockerfile              one image for both
├── skills.default/         shipped skills (git-tracked, read-only fallback)
├── skills/                 live overlay, gitignored; dreaming writes here
├── storage/                downloaded Gmail attachments (gitignored)
└── packages/
    ├── mcp/
    │   ├── data/{schema.sql, tokens.db}     OAuth, watermarks, signals, scheduled_tasks
    │   └── src/
    │       ├── server.ts                    starts pollers + HTTP/stdio transport
    │       ├── tools/                       MCP-exposed actions
    │       └── services/{gmail,telegram,monobank,scheduler,news,pdf,signals,settings}
    └── agent/
        ├── data/{schema.sql, agent.db}      agent-side state (memory KV)
        └── src/
            ├── supervisor.ts                main loop
            ├── engine.ts, session.ts        DeepSeek runner + synthetic tools
            ├── mcp-client.ts                StreamableHTTP client
            ├── session-context.ts           markdown context block builder
            ├── skills.ts                    two-layer loader (live → default)
            └── db/{client.ts, memory.ts}    KV helpers
```

## Stack

- TypeScript (ESM, `module: Preserve`, `moduleResolution: Bundler`)
- `@modelcontextprotocol/sdk` (stdio + StreamableHTTP transport)
- `better-sqlite3` for Node code; `sqlite3` CLI from Bash for ad-hoc queries
- `googleapis` + `google-auth-library` (Gmail)
- `telegram` (gramjs / MTProto) for userbot channel reading
- `cron-parser` v5 for scheduled tasks
- `openai` SDK pointed at DeepSeek (OpenAI-compatible)

## Two databases — split by ownership

- **`packages/mcp/data/tokens.db`** — MCP's private state. OAuth tokens,
  Gmail watermarks, Telegram poll cursors, the `signals` queue, the
  `scheduled_tasks` table, userbot channel watermarks. Don't read or write
  this from agent code — go through MCP tools.
- **`packages/agent/data/agent.db`** — agent's domain state. Currently
  only `memory` (freeform KV, e.g. `news_digest.last_read_at`). A `bills`
  table exists in schema as a leftover from earlier reconciliation work
  but is no longer populated.

Schemas: `packages/mcp/data/schema.sql`, `packages/agent/data/schema.sql`.
Re-apply with `pnpm db:init` (idempotent).

For ad-hoc queries during development:

```bash
sqlite3 -json packages/agent/data/agent.db "SELECT * FROM memory"
sqlite3      packages/agent/data/agent.db "UPDATE memory SET value=? WHERE key=?"
```

For multi-line / quote-heavy SQL, use a heredoc. Always single-quote
string literals; double single quotes inside (`'O''Brien'`).

## MCP tools (signal-emitting + agent-callable)

Defined in `packages/mcp/src/tools/`. The agent calls these via MCP; you
also see them when running `claude` locally with `.mcp.json` registered.

- **Gmail** — `list_nashdom_mails`, `download_gmail_attachment`
- **Telegram bot** — `send_telegram_message`, `edit_telegram_message`,
  `send_telegram_chat_action`, `get_telegram_chat_history`
- **Telegram userbot (read-only MTProto)** — `list_userbot_dialogs`,
  `list_channel_posts`
- **Monobank** — `list_monobank_transactions` (no poller; reactive only)
- **News** — `list_news_headlines`, `fetch_article` (HN, Habr)
- **PDF** — `read_pdf`
- **Files** — `read_file`
- **Signals queue** — `get_next_signal`, `list_signals`
- **Scheduler** — `schedule_task`, `list_scheduled_tasks`,
  `cancel_scheduled_task`
- **Env** — `get_timezone`, `set_timezone`

## Agent skills

Live under `skills.default/` (git-tracked, shipped in image) with an
optional live overlay in `skills/` (gitignored, mounted as a Docker
volume — written by the `dreaming` skill when it self-revises).

`loadSkill(name)` reads `skills/<name>.md` first, falls back to
`skills.default/<name>.md`. Naming is signal-source-based:

- `nashdom-bill`, `news-digest`, `tech-digest`, `dreaming`, `scheduler`,
  `telegram` — primary domain skills, loaded per signal.source.
- `routing`, `handoff` — always loaded on top.

## Running

- `pnpm db:init` — apply both schemas (mcp/tokens.db + agent/agent.db).
- `pnpm mcp:serve` — start the MCP server. **Do not run locally if the
  droplet is also running it** — Telegram getUpdates is exclusive and the
  second poller causes 409 Conflict.
- `pnpm agent:start` — start the supervisor (long-running loop).
- `pnpm gmail:auth` — one-time OAuth bootstrap. Writes to
  `packages/mcp/data/tokens.db`.
- `pnpm gmail:list-unread` — debug helper.
- `pnpm telegram:get-chat-id` — discover your chat id (after sending any
  message to your bot).
- `pnpm userbot:auth` — one-time MTProto login (phone + code).
- `pnpm typecheck` — typecheck both packages.

Deploy: see `docker-compose.yml`. `docker compose up -d --build` on the
droplet; named volumes (`mcp-data`, `mcp-storage`, `agent-data`,
`agent-skills`) persist state across rebuilds.
