-- MCP-side state: OAuth credentials + Telegram chat log (the bot is the
-- canonical Telegram surface, so message history lives here, not in the
-- agent DB). Re-run via `pnpm db:init:mcp`.

PRAGMA journal_mode = WAL;
PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS integration_account (
  provider      TEXT NOT NULL,
  account_key   TEXT NOT NULL,
  access_token  TEXT,
  refresh_token TEXT,
  expires_at    TEXT,
  metadata      TEXT,
  created_at    TEXT NOT NULL DEFAULT (datetime('now')),
  updated_at    TEXT NOT NULL DEFAULT (datetime('now')),
  PRIMARY KEY (provider, account_key)
);

-- Every message the bot sees or sends. role='user' for incoming,
-- role='assistant' for outgoing (recorded by send_telegram_message).
-- thread_id is the Telegram forum topic id (NULL for non-topic chats / general).
CREATE TABLE IF NOT EXISTS telegram_messages (
  id             INTEGER PRIMARY KEY AUTOINCREMENT,
  chat_id        INTEGER NOT NULL,
  tg_message_id  INTEGER,
  thread_id      INTEGER,
  role           TEXT NOT NULL CHECK (role IN ('user', 'assistant')),
  text           TEXT NOT NULL,
  created_at     TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE INDEX IF NOT EXISTS telegram_messages_chat_id_id ON telegram_messages(chat_id, id);
CREATE INDEX IF NOT EXISTS telegram_messages_chat_thread_id ON telegram_messages(chat_id, thread_id, id);

-- Internal scratch space for the poller (last_update_id, etc).
CREATE TABLE IF NOT EXISTS telegram_kv (
  key    TEXT PRIMARY KEY,
  value  TEXT NOT NULL
);

-- Gmail poller scratch space: per-subscription watermarks (last seen
-- internalDate in ms) so we don't re-emit signals for already-processed
-- emails across restarts.
CREATE TABLE IF NOT EXISTS gmail_kv (
  key    TEXT PRIMARY KEY,
  value  TEXT NOT NULL
);

-- News poller scratch space: last_digest_date so we fire the daily
-- tech-digest signal at most once per day across restarts.
CREATE TABLE IF NOT EXISTS news_kv (
  key    TEXT PRIMARY KEY,
  value  TEXT NOT NULL
);

-- Dreaming poller scratch space: stores `last_dreaming_at` (ISO timestamp).
-- Each dreaming run re-reads this watermark to decide which signals to
-- review and updates it on completion.
CREATE TABLE IF NOT EXISTS dreaming_kv (
  key    TEXT PRIMARY KEY,
  value  TEXT NOT NULL
);

-- News-digest poller scratch space (curated topical news from Telegram
-- channels — Odessa / PMR / RU-UA conflict / World).
CREATE TABLE IF NOT EXISTS news_digest_kv (
  key    TEXT PRIMARY KEY,
  value  TEXT NOT NULL
);

-- Signal queue. MCP-internal pollers (Telegram, Gmail, cron, webhooks)
-- enqueue rows; the agent consumes via get_next_signal which atomically
-- pops the oldest pending row. consumed_at IS NULL means pending.
CREATE TABLE IF NOT EXISTS signals (
  id           INTEGER PRIMARY KEY AUTOINCREMENT,
  source       TEXT NOT NULL,
  content      TEXT NOT NULL,
  created_at   TEXT NOT NULL DEFAULT (datetime('now')),
  consumed_at  TEXT
);
CREATE INDEX IF NOT EXISTS signals_pending ON signals(consumed_at, id);
