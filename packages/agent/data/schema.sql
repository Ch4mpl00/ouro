-- Agent-side state. Domain memory for the signal-driven supervisor.
-- Re-run via `pnpm db:init:agent`. Idempotent (IF NOT EXISTS).

PRAGMA journal_mode = WAL;
PRAGMA foreign_keys = ON;

-- Tracked utility bills extracted from NashDom emails.
-- One row per Gmail message (dedup on message_id).
CREATE TABLE IF NOT EXISTS bills (
  id                   INTEGER PRIMARY KEY AUTOINCREMENT,
  message_id           TEXT NOT NULL UNIQUE,
  subject              TEXT,
  "from"               TEXT,
  "date"               TEXT,                              -- email received date, YYYY-MM-DD
  invoice_date         TEXT,                              -- billing period, YYYY-MM
  account              TEXT,
  address              TEXT,
  type                 TEXT,
  amount               REAL,
  currency             TEXT,
  ibans                TEXT,                              -- JSON array of strings
  telegram_chat_id     TEXT,
  telegram_message_id  INTEGER,
  telegram_message_text TEXT,                              -- original notification body, for append-only edits
  paid                 INTEGER NOT NULL DEFAULT 0,        -- 0|1
  paid_at              TEXT,
  paid_transaction_id  TEXT,
  notes                TEXT,
  created_at           TEXT NOT NULL DEFAULT (datetime('now')),
  updated_at           TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE INDEX IF NOT EXISTS bills_paid ON bills(paid);

-- Freeform key-value store. Use for anything Claude wants to remember that
-- doesn't fit a typed table. Value is a JSON-stringified payload by convention.
CREATE TABLE IF NOT EXISTS memory (
  key         TEXT PRIMARY KEY,
  value       TEXT NOT NULL,
  updated_at  TEXT NOT NULL DEFAULT (datetime('now'))
);
