import { randomBytes } from "node:crypto";
import pg from "pg";
import { afterAll, beforeAll, describe, expect, it } from "vitest";
import { createPgClient, type PgClient } from "../../db/pg/client";
import type { EmbeddingService } from "../embeddings/service";
import { createIndexer } from "./indexer";
import { createMemoryService, type MemoryService } from "./service";
import { createPgMemoryStore } from "./store.pg";
import { testServerUrl } from "../../../test/support/database";

// The journal invariants only Postgres can break: append and trim are one
// transaction (I4), and appends racing on one stream on separate connections
// all land with the newest `capacity` surviving (I5). The in-memory store
// can't interleave, so these run against a throwaway database named by
// TEST_DATABASE_URL (never prod) or a container of our own; skipped without
// either.

// TEST_DATABASE_URL, or a container of our own when Docker is there.
const TEST_URL = await testServerUrl();
const ACTOR = "supervisor";

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

const noEmbeddings: EmbeddingService = {
  embed: async () => {
    throw new Error("the journal must not embed");
  },
  embedBatch: async () => {
    throw new Error("the journal must not embed");
  },
};

describe.skipIf(!TEST_URL)("journal on Postgres", () => {
  let client: PgClient;
  let service: MemoryService;

  beforeAll(async () => {
    const previous = process.env.DATABASE_URL;
    process.env.DATABASE_URL = await freshDatabase();
    client = createPgClient();
    process.env.DATABASE_URL = previous;
    await client.ensureReady();
    const store = createPgMemoryStore({ db: client.db });
    service = createMemoryService({ store, indexer: createIndexer({ store, embeddings: noEmbeddings }) });
  });

  afterAll(async () => {
    await client?.close();
  });

  it("I5 racing appends all land and exactly the newest `capacity` survive", async () => {
    const capacity = 5;
    await service.journalAppend({ stream: "race", body: "seed", actor: ACTOR, capacity });
    const results = await Promise.all(
      Array.from({ length: 40 }, (_, i) => service.journalAppend({ stream: "race", body: `p${i}`, actor: ACTOR })),
    );
    for (const r of results) expect(r.stream.size).toBeLessThanOrEqual(capacity);

    const { entries } = await service.journalTail({ stream: "race", n: 100 });
    const newest = results
      .map((r) => r.entry.id)
      .sort((a, b) => b - a)
      .slice(0, capacity);
    expect(entries.map((e) => e.id)).toEqual(newest);
  });

  it("I5 the first appends to a brand-new stream race safely too", async () => {
    await Promise.all(
      Array.from({ length: 20 }, (_, i) =>
        service.journalAppend({ stream: "born-racing", body: `p${i}`, actor: ACTOR, capacity: 3 }),
      ),
    );
    const [stream] = (await service.listJournals()).filter((s) => s.name === "born-racing");
    expect(stream).toMatchObject({ capacity: 3, size: 3 });
  });

  it("I2 tail is newest-first and round-trips the body byte for byte", async () => {
    const body = "  multi\nline «тело» ё  \n";
    await service.journalAppend({ stream: "order", body: "older", actor: ACTOR });
    await service.journalAppend({ stream: "order", body, actor: ACTOR });
    const { entries } = await service.journalTail({ stream: "order", n: 2 });
    expect(entries.map((e) => e.body)).toEqual([body, "older"]);
  });

  it("F4 lowering the capacity trims on that append", async () => {
    for (let i = 0; i < 6; i++) await service.journalAppend({ stream: "shrink", body: `e${i}`, actor: ACTOR });
    const out = await service.journalAppend({ stream: "shrink", body: "last", actor: ACTOR, capacity: 2 });
    expect(out.stream).toMatchObject({ capacity: 2, size: 2 });
    const { entries } = await service.journalTail({ stream: "shrink", n: 10 });
    expect(entries.map((e) => e.body)).toEqual(["last", "e5"]);
  });
});
