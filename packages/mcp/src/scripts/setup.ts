import path from "node:path";
import { mkdirSync, readFileSync } from "node:fs";
import { getDb, closeDb } from "../db/client";

// Idempotent setup. Two-phase:
//   1. getDb() — opens connection and runs the in-code migrations
//      (ALTER TABLE etc) for existing DBs that pre-date a schema change.
//      For brand-new DBs it's a no-op.
//   2. Apply schema.sql — base CREATE TABLE statements (all IF NOT EXISTS).
//      Safe to run every container boot.
//
// Order matters: schema.sql references columns added by migrations
// (e.g. telegram_messages.thread_id) in its index definitions. Migrations
// must land first.

const DATA_DIR = path.resolve(import.meta.dirname, "../../data");
const DB_PATH = process.env.MCP_DB_PATH ?? path.join(DATA_DIR, "tokens.db");
const SCHEMA_PATH = path.join(DATA_DIR, "schema.sql");

mkdirSync(path.dirname(DB_PATH), { recursive: true });

const db = getDb();
const schema = readFileSync(SCHEMA_PATH, "utf-8");
db.exec(schema);
closeDb();

console.log(`[setup:mcp] schema applied to ${DB_PATH}`);
