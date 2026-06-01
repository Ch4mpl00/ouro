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
        // Run outside the migration files so they don't have to assume
        // CREATE EXTENSION privileges (e.g. when applied by managed PG).
        await pool.query("CREATE EXTENSION IF NOT EXISTS vector");
        await migrate(db, { migrationsFolder: MIGRATIONS_DIR });
        console.log("[mcp-pg] migrations applied, news store ready");
      })();
    }
    return ready;
  };

  const close = (): Promise<void> => pool.end();

  return { db, ensureReady, close };
}

export { schema };
