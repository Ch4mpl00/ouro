import path from "node:path";
import { mkdirSync, readFileSync } from "node:fs";
import Database from "better-sqlite3";

// Applies the MCP schema to packages/mcp/data/tokens.db. Idempotent — every
// CREATE in schema.sql is `IF NOT EXISTS`. Safe to run on every container
// boot via the entrypoint command (`pnpm setup:mcp && pnpm mcp:serve`).
// Replaces the previous `sqlite3` CLI-based `db:init:mcp`, removing the
// runtime dependency on the sqlite3 binary inside the container.

const DATA_DIR = path.resolve(import.meta.dirname, "../../data");
const DB_PATH = process.env.MCP_DB_PATH ?? path.join(DATA_DIR, "tokens.db");
const SCHEMA_PATH = path.join(DATA_DIR, "schema.sql");

mkdirSync(path.dirname(DB_PATH), { recursive: true });

const db = new Database(DB_PATH);
db.pragma("journal_mode = WAL");
db.pragma("foreign_keys = ON");

const schema = readFileSync(SCHEMA_PATH, "utf-8");
db.exec(schema);
db.close();

console.log(`[setup:mcp] applied schema to ${DB_PATH}`);
