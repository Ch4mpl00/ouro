import path from "node:path";
import Database from "better-sqlite3";

// Singleton sqlite handle for the agent's domain state (bills, memory).
// Default path is resolved relative to this source file so the lookup works
// regardless of cwd.

declare global {
  // eslint-disable-next-line no-var
  var __agent_db: Database.Database | undefined;
}

const DEFAULT_PATH = path.resolve(import.meta.dirname, "../../data/agent.db");

function dbPath(): string {
  return process.env.AGENT_DB_PATH ?? DEFAULT_PATH;
}

export function getDb(): Database.Database {
  if (!global.__agent_db) {
    const db = new Database(dbPath());
    db.pragma("journal_mode = WAL");
    db.pragma("foreign_keys = ON");
    global.__agent_db = db;
  }
  return global.__agent_db;
}

export function closeDb(): void {
  if (global.__agent_db) {
    global.__agent_db.close();
    global.__agent_db = undefined;
  }
}
