// A throwaway Postgres for one integration suite.
//
//   - TEST_DATABASE_URL named → fresh databases inside that server (news store
//     + its own mcp_state), dropped afterwards. Never point it at prod.
//   - otherwise → a Testcontainers pgvector container, stopped afterwards.
//
// `databaseAvailable()` is false with neither, so suites skip instead of
// failing on a machine without Docker.

import { randomBytes } from "node:crypto";
import { PostgreSqlContainer } from "@testcontainers/postgresql";
import pg from "pg";
import { getContainerRuntimeClient } from "testcontainers";

export async function databaseAvailable(): Promise<boolean> {
  if (process.env.TEST_DATABASE_URL) return true;
  try {
    await getContainerRuntimeClient();
    return true;
  } catch {
    return false;
  }
}

export interface TestDatabase {
  // The news-store URL a server boots from (DATABASE_URL).
  url: string;
  drop(): Promise<void>;
}

// Sets STATE_DATABASE_URL so the server's mcp_state is private to this suite
// too; vitest runs each file in its own process, so the env is ours.
export async function testDatabase(): Promise<TestDatabase> {
  const named = process.env.TEST_DATABASE_URL;
  if (!named) {
    const container = await new PostgreSqlContainer("pgvector/pgvector:pg16").start();
    delete process.env.STATE_DATABASE_URL;
    return { url: container.getConnectionUri(), drop: async () => void (await container.stop()) };
  }
  const name = `t_${randomBytes(8).toString("hex")}`;
  await admin(named, `CREATE DATABASE ${name}`);
  process.env.STATE_DATABASE_URL = withDatabase(named, `${name}_state`);
  return {
    url: withDatabase(named, name),
    drop: async () => {
      await admin(named, `DROP DATABASE IF EXISTS ${name}_state WITH (FORCE)`);
      await admin(named, `DROP DATABASE IF EXISTS ${name} WITH (FORCE)`);
    },
  };
}

function withDatabase(serverUrl: string, name: string): string {
  const url = new URL(serverUrl);
  url.pathname = `/${name}`;
  return url.toString();
}

async function admin(serverUrl: string, sql: string): Promise<void> {
  const client = new pg.Client({ connectionString: serverUrl });
  await client.connect();
  try {
    await client.query(sql);
  } finally {
    await client.end();
  }
}

// The server URL for suites that create their own databases on it:
// TEST_DATABASE_URL when named, otherwise a container started for this file
// (Testcontainers' reaper removes it when the process exits). undefined
// without either, so the suite skips.
export async function testServerUrl(): Promise<string | undefined> {
  const named = process.env.TEST_DATABASE_URL;
  if (named) return named;
  if (!(await databaseAvailable())) return undefined;
  const container = await new PostgreSqlContainer("pgvector/pgvector:pg16").start();
  return container.getConnectionUri();
}
