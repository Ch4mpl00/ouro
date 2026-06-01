# Centralized config (zod-validated)

**Status:** pending
**Priority:** P1
**Area:** infra / config
**Created:** 2026-06-01

## Context

`process.env.*` reads are scattered: `OPENAI_API_KEY` in
`embeddings/provider.ts`, `DATABASE_URL` in `db/pg/client.ts`,
`USERBOT_POLL_INTERVAL_MS` in `userbot/poller.ts`,
`TELEGRAM_*` / `MONOBANK_API_KEY` in various services. Failure modes
manifest only on first use of each component — adding a new env var
means hunting through code to find where it's parsed and validated.
There's no single place a new dev or a deploy-time check can look at
to know what envs are required.

## Acceptance

- `src/config.ts` exporting `loadConfig(): Config` that parses
  `process.env` with zod and returns a typed object.
- All env reads in business code removed; values come from `config`
  parameter on factories (`createPgClient({ url })`,
  `createOpenAIProvider({ apiKey })`, etc).
- Validation failures crash on startup with a clear message listing
  every missing/invalid var — not at first request.
- `loadConfig()` runs once in `main()` and is passed via DI.
- Config schema covers: `DATABASE_URL`, `OPENAI_API_KEY`,
  `TELEGRAM_ASSISTANT_BOT_TOKEN`, `TELEGRAM_DEFAULT_CHAT_ID`,
  `TELEGRAM_APP_ID/HASH`, `MONOBANK_API_KEY`,
  `USERBOT_POLL_INTERVAL_MS`, `MCP_TRANSPORT`, `MCP_PORT`,
  `STORAGE_DIR`, Google OAuth trio, optional Langfuse trio,
  optional `TELEGRAM_TOPICS_JSON`.

## Notes

`zod` is already a dep (used for MCP tool input schemas), so no new
package needed.

Booleans/numbers should `.coerce`-cast from string — env vars are
always strings.
