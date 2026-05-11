import path from "node:path";
import Database from "better-sqlite3";

// Singleton sqlite handle for MCP-owned state (OAuth tokens). The MCP server
// is otherwise stateless and does not see the agent's DB. Default path is
// resolved relative to this source file so the lookup works regardless of
// cwd (the server may be launched from the repo root via .mcp.json).

declare global {
  // eslint-disable-next-line no-var
  var __mcp_db: Database.Database | undefined;
}

const DEFAULT_PATH = path.resolve(import.meta.dirname, "../../data/tokens.db");

function dbPath(): string {
  return process.env.MCP_DB_PATH ?? DEFAULT_PATH;
}

export function getDb(): Database.Database {
  if (!global.__mcp_db) {
    const db = new Database(dbPath());
    db.pragma("journal_mode = WAL");
    db.pragma("foreign_keys = ON");
    runMigrations(db);
    global.__mcp_db = db;
  }
  return global.__mcp_db;
}

// Idempotent at-startup migrations for additive schema changes that aren't
// covered by `CREATE TABLE IF NOT EXISTS` (i.e. ALTER TABLE for new columns).
// Re-running schema.sql on existing DBs is a no-op for these.
function runMigrations(db: Database.Database): void {
  const tableExists = db
    .prepare(`SELECT 1 FROM sqlite_master WHERE type='table' AND name=?`)
    .get("telegram_messages");
  if (tableExists) {
    const cols = db.prepare(`PRAGMA table_info(telegram_messages)`).all() as { name: string }[];
    if (!cols.some((c) => c.name === "thread_id")) {
      db.exec(`ALTER TABLE telegram_messages ADD COLUMN thread_id INTEGER`);
      db.exec(
        `CREATE INDEX IF NOT EXISTS telegram_messages_chat_thread_id ON telegram_messages(chat_id, thread_id, id)`,
      );
    }
  }
  db.exec(`
    CREATE TABLE IF NOT EXISTS news_kv (
      key   TEXT PRIMARY KEY,
      value TEXT NOT NULL
    )
  `);
  db.exec(`
    CREATE TABLE IF NOT EXISTS dreaming_kv (
      key   TEXT PRIMARY KEY,
      value TEXT NOT NULL
    )
  `);
  db.exec(`
    CREATE TABLE IF NOT EXISTS news_digest_kv (
      key   TEXT PRIMARY KEY,
      value TEXT NOT NULL
    )
  `);
}

export function closeDb(): void {
  if (global.__mcp_db) {
    global.__mcp_db.close();
    global.__mcp_db = undefined;
  }
}
