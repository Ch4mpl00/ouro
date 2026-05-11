import path from "node:path";
import { mkdirSync, readFileSync } from "node:fs";
import Database from "better-sqlite3";

// Applies the agent schema to packages/agent/data/agent.db. Idempotent.
// Run on container boot via the entrypoint (`pnpm setup:agent && pnpm agent:start`).

const DATA_DIR = path.resolve(import.meta.dirname, "../../data");
const DB_PATH = process.env.AGENT_DB_PATH ?? path.join(DATA_DIR, "agent.db");
const SCHEMA_PATH = path.join(DATA_DIR, "schema.sql");

mkdirSync(path.dirname(DB_PATH), { recursive: true });

const db = new Database(DB_PATH);
db.pragma("journal_mode = WAL");
db.pragma("foreign_keys = ON");

const schema = readFileSync(SCHEMA_PATH, "utf-8");
db.exec(schema);
db.close();

console.log(`[setup:agent] applied schema to ${DB_PATH}`);
