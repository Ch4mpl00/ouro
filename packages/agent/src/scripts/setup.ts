import path from "node:path";
import { mkdirSync } from "node:fs";
import { createAgentDb } from "../db/client";

// Apply pending Drizzle migrations to agent.db. Opening the handle runs the
// migrator (see db/client.ts), so this is just: ensure the data dir exists,
// open (→ migrate), close. Migrations are baked into the image under
// src/db/migrations — NOT the agent-data volume — so they actually ship.

const DATA_DIR = path.resolve(import.meta.dirname, "../../data");
const DB_PATH = process.env.AGENT_DB_PATH ?? path.join(DATA_DIR, "agent.db");

mkdirSync(path.dirname(DB_PATH), { recursive: true });

const db = createAgentDb(DB_PATH);
db.$client.close();

console.log(`[setup:agent] migrations applied to ${DB_PATH}`);
