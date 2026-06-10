import path from "node:path";
import { mkdirSync, readFileSync } from "node:fs";
import { createAgentDb } from "../db/client";

const DATA_DIR = path.resolve(import.meta.dirname, "../../data");
const DB_PATH = process.env.AGENT_DB_PATH ?? path.join(DATA_DIR, "agent.db");
const SCHEMA_PATH = path.join(DATA_DIR, "schema.sql");

mkdirSync(path.dirname(DB_PATH), { recursive: true });

const db = createAgentDb(DB_PATH);
const schema = readFileSync(SCHEMA_PATH, "utf-8");
db.exec(schema);
db.close();

console.log(`[setup:agent] schema applied to ${DB_PATH}`);
