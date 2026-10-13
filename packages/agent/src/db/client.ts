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
  // Several processes (supervisor, judge-worker) open the same DB and migrate
  // at startup; a busy_timeout lets the loser of the migration race wait out
  // the writer's transaction instead of throwing SQLITE_BUSY. It goes first:
  // switching a fresh file to WAL takes a lock too, and set after it, a
  // process opening alongside another failed with "database is locked".
  sqlite.pragma("busy_timeout = 5000");
  // Turning a fresh file into WAL is not covered by the busy handler: a
  // process doing it alongside another gets SQLITE_BUSY at once.
  retryBootRace(() => sqlite.pragma("journal_mode = WAL"));
  sqlite.pragma("foreign_keys = ON");

  const db = drizzle(sqlite, { schema });
  // Idempotent: applies any pending migrations, no-op once up to date. Sync
  // for better-sqlite3, so callers just get a ready-to-use handle.
  migrateRacingOthers(db);
  return db;
}

export { schema };

// Services that share the volume (agent, judge-worker, improve-worker) boot
// at once after a deploy with a new migration. Drizzle reads the last applied
// migration before its BEGIN, so they all see it pending; one commits, the
// others' CREATE fails with "already exists" — and their whole run rolls back
// (one transaction). Running migrate again re-reads the record the winner
// left and skips it. (.claude/tasks/agent-migration-race.md)
function migrateRacingOthers(db: AgentDatabase): void {
  retryBootRace(() => migrate(db, { migrationsFolder: MIGRATIONS_DIR }));
}

// Retries a boot step that lost a race with another process opening the same
// file: "database is locked" (the WAL switch) or "already exists" (a
// migration someone else just applied). Anything else, or a race lost
// MAX_BOOT_ATTEMPTS times, is thrown.
const MAX_BOOT_ATTEMPTS = 8;

function retryBootRace(step: () => unknown): void {
  for (let attempt = 1; ; attempt++) {
    try {
      step();
      return;
    } catch (err) {
      const text = err instanceof Error ? `${err.message} ${String(err.cause)}` : String(err);
      if (!/database is locked|already exists/.test(text) || attempt >= MAX_BOOT_ATTEMPTS) throw err;
      Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, 25 * attempt + Math.floor(Math.random() * 25));
    }
  }
}
