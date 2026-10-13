import { randomBytes } from "node:crypto";
import pg from "pg";
import { describe, expect, it } from "vitest";
import { createPgClient } from "./client";
import { testServerUrl } from "../../../test/support/database";

// Runs against a throwaway database named by TEST_DATABASE_URL (never prod),
// or a container; skipped without either. Each test gets a fresh database on that server.

// TEST_DATABASE_URL, or a container of our own when Docker is there.
const TEST_URL = await testServerUrl();

async function freshDatabase(): Promise<string> {
  const name = `t_${randomBytes(8).toString("hex")}`;
  const admin = new pg.Client({ connectionString: TEST_URL });
  await admin.connect();
  await admin.query(`CREATE DATABASE ${name}`);
  await admin.end();
  const url = new URL(TEST_URL!);
  url.pathname = `/${name}`;
  return url.toString();
}

describe.skipIf(!TEST_URL)("pg migrations", () => {
  // mcp and mcp-tunnel boot at the same moment. Without the advisory lock
  // both run the DDL and the loser fails on "relation already exists".
  it("lets two instances boot against one fresh database at once", async () => {
    const url = await freshDatabase();
    const previous = process.env.DATABASE_URL;
    process.env.DATABASE_URL = url;
    const [a, b] = [createPgClient(), createPgClient()];
    process.env.DATABASE_URL = previous;
    try {
      await Promise.all([a.ensureReady(), b.ensureReady()]);
    } finally {
      await Promise.all([a.close(), b.close()]);
    }

    const check = new pg.Client({ connectionString: url });
    await check.connect();
    const { rows } = await check.query<{ hash: string; created_at: string }>(
      `SELECT hash, created_at FROM drizzle.__drizzle_migrations ORDER BY created_at`,
    );
    await check.end();
    // The journal the Rust server writes: same files, same hashes, same order,
    // so either server recognises a database the other migrated.
    expect(rows.map((r) => [r.hash.slice(0, 16), Number(r.created_at)])).toEqual([
      ["6fe80692cd40ace1", 1780238110688],
      ["5829fed5a52485b9", 1780326897486],
      ["2d868dbf377baa83", 1781001341744],
      ["563cc5c374b93cd7", 1787496077308],
      ["ede5123f9e74197b", 1790957842163],
      ["5114e44328f417de", 1791823358054],
      ["4b9f63ec641fecd7", 1791902517698],
    ]);
  });
});
