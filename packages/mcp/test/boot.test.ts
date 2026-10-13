// Booting against Postgres when several MCP instances start at once (mcp and
// mcp-tunnel do, on every deploy): the state database and the news store are
// created, migrated and seeded exactly once, every instance comes up, and a
// later boot changes nothing.

import { randomBytes } from "node:crypto";
import fc from "fast-check";
import pg from "pg";
import { afterAll, beforeAll, describe, expect, it } from "vitest";
import { createPgClient } from "../src/db/pg/client";
import { connectStateDb } from "../src/db/state";
import { databaseAvailable, testDatabase, type TestDatabase } from "./support/database";

const available = await databaseAvailable();

function withDatabase(serverUrl: string, name: string): string {
  const url = new URL(serverUrl);
  url.pathname = `/${name}`;
  return url.toString();
}

async function query<T extends pg.QueryResultRow>(url: string, sql: string): Promise<T[]> {
  const c = new pg.Client({ connectionString: url });
  await c.connect();
  try {
    return (await c.query<T>(sql)).rows;
  } finally {
    await c.end();
  }
}

describe.skipIf(!available)("booting several instances at once", () => {
  let db: TestDatabase;

  beforeAll(async () => {
    db = await testDatabase();
  }, 180_000);
  afterAll(async () => {
    await db?.drop();
  });

  it("the state database is created, migrated and seeded once, and every instance comes up", async () => {
    await fc.assert(
      fc.asyncProperty(fc.integer({ min: 2, max: 6 }), async (instances) => {
        const stateUrl = withDatabase(db.url, `state_${randomBytes(6).toString("hex")}`);
        const booted = await Promise.allSettled(Array.from({ length: instances }, () => connectStateDb(db.url, stateUrl)));
        const failures = booted.flatMap((b) => (b.status === "rejected" ? [String(b.reason)] : []));
        for (const b of booted) if (b.status === "fulfilled") await b.value.close();
        expect(failures, `${instances} instances booting at once`).toEqual([]);

        const migrations = await query<{ version: number }>(stateUrl, "SELECT version FROM state_migrations ORDER BY version");
        expect(migrations.map((m) => m.version), "each migration recorded once").toEqual([...new Set(migrations.map((m) => m.version))]);
        const seeded = await query<{ n: string }>(stateUrl, "SELECT count(*) AS n FROM scheduled_tasks");
        const seededOnce = Number(seeded[0]?.n);

        // A later boot finds everything in place and changes nothing.
        const again = await connectStateDb(db.url, stateUrl);
        await again.close();
        const after = await query<{ n: string }>(stateUrl, "SELECT count(*) AS n FROM scheduled_tasks");
        expect(Number(after[0]?.n), "a reboot seeds nothing again").toBe(seededOnce);
        expect(seededOnce, "the system tasks, once").toBe(3);
      }),
      { numRuns: 12 },
    );
  }, 300_000);

  it("the news store migrates once with several instances racing", async () => {
    await fc.assert(
      fc.asyncProperty(fc.integer({ min: 2, max: 5 }), async (instances) => {
        const name = `news_${randomBytes(6).toString("hex")}`;
        await query(db.url, `CREATE DATABASE ${name}`);
        const url = withDatabase(db.url, name);
        const previous = process.env.DATABASE_URL;
        process.env.DATABASE_URL = url;
        const clients = Array.from({ length: instances }, () => createPgClient());
        process.env.DATABASE_URL = previous;
        try {
          const booted = await Promise.allSettled(clients.map((c) => c.ensureReady()));
          expect(booted.filter((b) => b.status === "rejected").map((b) => (b.status === "rejected" ? String(b.reason) : "")), `${instances} instances`).toEqual([]);
        } finally {
          await Promise.all(clients.map((c) => c.close()));
        }
        const rows = await query<{ hash: string }>(url, "SELECT hash FROM drizzle.__drizzle_migrations");
        expect(rows.length, "each migration recorded once").toBe(new Set(rows.map((r) => r.hash)).size);
      }),
      { numRuns: 8 },
    );
  }, 300_000);
});
