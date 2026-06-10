import path from "node:path";
import Database from "better-sqlite3";

// Sqlite handle for the agent's domain state (memory KV). Built once in the
// composition root (supervisor main / a script's main) and passed down —
// no module-level singleton, per the workspace DI rules. Default path is
// resolved relative to this source file so the lookup works regardless of
// cwd.

const DEFAULT_PATH = path.resolve(import.meta.dirname, "../../data/agent.db");

export function createAgentDb(dbPath?: string): Database.Database {
  const db = new Database(dbPath ?? process.env.AGENT_DB_PATH ?? DEFAULT_PATH);
  db.pragma("journal_mode = WAL");
  db.pragma("foreign_keys = ON");
  return db;
}
