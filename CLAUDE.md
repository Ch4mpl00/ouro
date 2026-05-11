# mcp-tools

A personal-agent system structured as two independent projects in one pnpm workspace:

- **`packages/mcp`** — stateless MCP server. Wraps Gmail / Telegram / Monobank as primitive tools. Knows nothing about the agent.
- **`packages/agent`** — agent harness. DB (domain memory) + heartbeat scheduler. Knows nothing about MCP internals — only the actions exposed by the MCP protocol.

Both communicate strictly through the MCP protocol when Claude is invoked. Neither imports code from the other.

## How a tick happens

1. **Heartbeat** (`pnpm heartbeat:start`) fires every minute (`HEARTBEAT_INTERVAL_MS`).
2. The heartbeat spawns one `claude -p` (Haiku 4.5 by default — cheap, capable) pointed at `HEARTBEAT.md`.
3. Claude reads `HEARTBEAT.md` fresh, runs the listed jobs (each with a precondition so most ticks are no-ops), and replies with a one-line summary.
4. **Agent jobs grow by editing `HEARTBEAT.md`** — add a numbered job with a precondition + action. No code change in the heartbeat. Claude re-reads the file fresh on every tick.

## Layout

```
mcp-tools/
├── pnpm-workspace.yaml
├── package.json            workspace orchestration scripts only
├── tsconfig.json           shared TS config (covers both packages)
├── .mcp.json               registers packages/mcp with Claude Code
├── .env, .env.example      shared by both packages
├── HEARTBEAT.md            heartbeat instructions (per-tick job list); edit to extend
├── .claude/skills/         {check-nashdom-bills, reconcile-bill-payments}
├── storage/                downloaded Gmail attachments (gitignored)
└── packages/
    ├── mcp/
    │   ├── data/{schema.sql, tokens.db}        OAuth state only
    │   └── src/{server, result, db, tools/*, services/{gmail,telegram,monobank}}
    └── agent/
        ├── data/{schema.sql, agent.db}         domain state (bills, memory)
        └── src/{db, heartbeat/start.ts}
```

## Stack

- TypeScript (ESM, `module: Preserve`, `moduleResolution: Bundler`)
- `@modelcontextprotocol/sdk` (stdio transport, in `packages/mcp`)
- `better-sqlite3` for Node code; `sqlite3` CLI from Bash for Claude
- `googleapis` + `google-auth-library` (Gmail, read-only)

## DB context (Claude's memory)

Two databases. **Claude's domain state lives in `packages/agent/data/agent.db`.** MCP's `tokens.db` is internal — don't read or write it from skills.

Schema: see `packages/agent/data/schema.sql`. Re-apply with `pnpm db:init:agent` (idempotent).

Tables in `agent.db`:

- **`bills`** — tracked NashDom bills. Dedup on `message_id`. Fields: `id`, `message_id`, `subject`, `from`, `date`, `invoice_date` (YYYY-MM), `account`, `address`, `type`, `amount`, `currency`, `ibans` (JSON array string), `telegram_chat_id`, `telegram_message_id`, `paid` (0|1), `paid_at`, `paid_transaction_id`, `notes`, `created_at`, `updated_at`. The reconciler writes `paid*`; everything else is set on first ingestion.
- **`memory`** — freeform `(key, value)` KV. Use for anything you want to remember that doesn't fit a typed table (e.g. "last seen Monobank txn id"). `value` is a JSON string by convention.

Query conventions:

```bash
sqlite3 -json packages/agent/data/agent.db "SELECT ... FROM bills WHERE paid = 0"
sqlite3      packages/agent/data/agent.db "UPDATE bills SET paid = 1, paid_at = datetime('now') WHERE id = 7"
```

For multi-line / quote-heavy SQL, use a heredoc. Always single-quote string literals; double single quotes inside (`'O''Brien'`).

## MCP tools (exposed actions)

- `list_nashdom_mails(limit?)` — unread NashDom emails with PDF attachments. No side effects.
- `download_gmail_attachment(messageId, attachmentId, filename?)` — saves to `./storage/gmail/<account>/<messageId>/...`, returns absolute filePath.
- `send_telegram_message(text, chatId?)` — send via assistant bot. Returns the Telegram `messageId` — persist it on the relevant bill row so the reconciler can later edit-in-place.
- `edit_telegram_message(chatId, messageId, text)` — edit a previously-sent message (e.g. mark a bill PAID).
- `list_monobank_transactions(accountId?, days?)` — recent transactions. Default account `'0'`, default 7d, max 31d. Rate-limited 1 req / 60s / account.

## Skills

- `check-nashdom-bills` — ingest new bills from Gmail into `agent.db` and notify via Telegram. Used by HEARTBEAT.md job #1 and also user-invocable.
- `reconcile-bill-payments` — match Monobank txns against unpaid bills, mark paid, update Telegram. Used by HEARTBEAT.md job #2 and also user-invocable.

## Running

- `pnpm db:init` — apply both schemas (mcp/tokens.db + agent/agent.db). Idempotent.
- `pnpm mcp:serve` — start the integrations MCP (also auto-launched by Claude Code via `.mcp.json`).
- `pnpm heartbeat:start` — long-running agent loop. Keep alive in tmux/launchd.
- `pnpm gmail:auth` — one-time OAuth bootstrap. Writes to `packages/mcp/data/tokens.db`.
- `pnpm gmail:list-unread` — debug helper.
- `pnpm telegram:get-chat-id` — discover your chat id (after sending any message to your bot).
- `pnpm typecheck` — typecheck both packages.

## Heartbeat env

- `HEARTBEAT_INTERVAL_MS` (default `60000`) — tick interval.
- `HEARTBEAT_TIMEOUT_MS` (default `300000`) — per-tick max wall time.
- `HEARTBEAT_MODEL` (default `claude-haiku-4-5-20251001`) — model spawned every tick. Switch to `claude-sonnet-4-6` if you need more reasoning power.
