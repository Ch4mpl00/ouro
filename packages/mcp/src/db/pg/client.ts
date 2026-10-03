import path from "node:path";
import pg from "pg";
import { drizzle, type NodePgDatabase } from "drizzle-orm/node-postgres";
import { migrate } from "drizzle-orm/node-postgres/migrator";
import * as schema from "./schema";

export type Database = NodePgDatabase<typeof schema>;

export interface PgClient {
  db: Database;
  ensureReady(): Promise<void>;
  close(): Promise<void>;
}

const MIGRATIONS_DIR = path.resolve(import.meta.dirname, "./migrations");

// `mcp` and `mcp-tunnel` boot against one cluster at the same moment;
// unserialised they race through the same DDL. The second instance waits
// on this lock, then finds nothing left to apply. "mcp_mig" — the key the
// Rust server takes too.
const MIGRATION_LOCK = "30790106749954407";

function databaseUrl(): string {
  const url = process.env.DATABASE_URL;
  if (!url) {
    throw new Error(
      "DATABASE_URL is not set. The mcp container needs Postgres for the news/RAG store; see .env.postgres.example.",
    );
  }
  return url;
}

export function createPgClient(): PgClient {
  const pool = new pg.Pool({ connectionString: databaseUrl() });
  const db = drizzle(pool, { schema });

  let ready: Promise<void> | null = null;
  const ensureReady = (): Promise<void> => {
    if (!ready) {
      ready = (async () => {
        // A session-level lock lives on one connection, so the migrator runs
        // on that same connection.
        const client = await pool.connect();
        try {
          await client.query("SELECT pg_advisory_lock($1)", [MIGRATION_LOCK]);
          try {
            // Run outside the migration files so they don't have to assume
            // CREATE EXTENSION privileges (e.g. when applied by managed PG).
            await client.query("CREATE EXTENSION IF NOT EXISTS vector");
            await migrate(drizzle(client, { schema }), { migrationsFolder: MIGRATIONS_DIR });
          } finally {
            await client.query("SELECT pg_advisory_unlock($1)", [MIGRATION_LOCK]);
          }
        } finally {
          client.release();
        }
        console.log("[mcp-pg] migrations applied, news store ready");
      })();
    }
    return ready;
  };

  const close = (): Promise<void> => pool.end();

  return { db, ensureReady, close };
}

export { schema };
