import "dotenv/config";
import { createPgClient, databaseUrl } from "../db/pg/client";
import { connectStateDb } from "../db/state";

// Idempotent setup: migrates the news/memory store and creates + migrates +
// seeds the `mcp_state` database. The server does the same on boot; this is
// for preparing a database without starting it.

const pg = createPgClient();
try {
  await pg.ensureReady();
  const state = await connectStateDb(databaseUrl());
  await state.close();
  console.log("[setup:mcp] news store and mcp_state are migrated");
} finally {
  await pg.close();
}
