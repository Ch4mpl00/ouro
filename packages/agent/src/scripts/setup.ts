import path from "node:path";
import { mkdirSync, readFileSync } from "node:fs";
import { getDb, closeDb } from "../db/client";

const DATA_DIR = path.resolve(import.meta.dirname, "../../data");
const DB_PATH = process.env.AGENT_DB_PATH ?? path.join(DATA_DIR, "agent.db");
const SCHEMA_PATH = path.join(DATA_DIR, "schema.sql");

mkdirSync(path.dirname(DB_PATH), { recursive: true });

const db = getDb();
const schema = readFileSync(SCHEMA_PATH, "utf-8");
db.exec(schema);
closeDb();

console.log(`[setup:agent] schema applied to ${DB_PATH}`);
