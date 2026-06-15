import path from "node:path";
import Database from "better-sqlite3";
import { drizzle, type BetterSQLite3Database } from "drizzle-orm/better-sqlite3";
import { migrate } from "drizzle-orm/better-sqlite3/migrator";
import * as schema from "./schema";

// `drizzle()` augments the base class with `$client` (the raw better-sqlite3
// handle, used for `.close()`); the base type alone doesn't carry it.

// Drizzle handle for the agent's domain state (memory KV + the local trace /
// judgement mirror), in agent.db. Built once in the composition root
// (supervisor main / a script's main) and passed down — no module-level
// singleton, per the workspace DI rules. Mirrors the MCP-side Drizzle setup
// (packages/mcp/src/db/pg/client.ts), only for sqlite.

export type AgentDatabase = BetterSQLite3Database<typeof schema> & {
  $client: Database.Database;
};

const DEFAULT_PATH = path.resolve(import.meta.dirname, "../../data/agent.db");
// Migrations live in source (baked into the image), NOT under the agent-data
// volume — so a schema change actually ships, instead of being shadowed by the
// volume's stale copy the way the old data/schema.sql was.
const MIGRATIONS_DIR = path.resolve(import.meta.dirname, "./migrations");

export function createAgentDb(dbPath?: string): AgentDatabase {
  const sqlite = new Database(dbPath ?? process.env.AGENT_DB_PATH ?? DEFAULT_PATH);
  sqlite.pragma("journal_mode = WAL");
  sqlite.pragma("foreign_keys = ON");
  // Several processes (supervisor, judge-worker) open the same DB and migrate
  // at startup; a busy_timeout lets the loser of the migration race wait out
  // the writer's transaction instead of throwing SQLITE_BUSY.
  sqlite.pragma("busy_timeout = 5000");

  const db = drizzle(sqlite, { schema });
  // Idempotent: applies any pending migrations, no-op once up to date. Sync
  // for better-sqlite3, so callers just get a ready-to-use handle.
  migrate(db, { migrationsFolder: MIGRATIONS_DIR });
  return db;
}

export { schema };
