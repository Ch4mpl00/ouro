import { randomBytes } from "node:crypto";
import pg from "pg";
import { afterAll, beforeAll, describe, expect, it } from "vitest";
import { createPgClient, type PgClient } from "../../../db/pg/client";
import { newsItems } from "../../../db/pg/schema";
import type { EmbeddingService } from "../../embeddings/service";
import { createNewsEmbedder } from "./embedder";
import { createNewsRepository, searchPool, type NewsRepository } from "./repository";
import { testServerUrl } from "../../../../test/support/database";

// Vector search against a real pgvector: the pool is ordered by the
// expression the HNSW index covers (an exact `<=>` on the full vector is a
// sequential scan over every embedding — 9.5 s at 117k rows on prod), while
// the distance a caller sees stays exact. TEST_DATABASE_URL (never prod) or a
// container of our own; skipped without either.

const TEST_URL = await testServerUrl();
const DIMS = 1536;

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

// Deterministic unit vectors: a seeded LCG, so a failure reproduces.
function unitVectors(count: number, seed: number): number[][] {
  let state = seed;
  const next = () => {
    state = (state * 1103515245 + 12345) % 2 ** 31;
    return state / 2 ** 31 - 0.5;
  };
  return Array.from({ length: count }, () => {
    const v = Array.from({ length: DIMS }, next);
    const norm = Math.hypot(...v);
    return v.map((x) => x / norm);
  });
}

function cosineDistance(a: number[], b: number[]): number {
  let dot = 0;
  for (let i = 0; i < a.length; i++) dot += a[i]! * b[i]!;
  return 1 - dot;
}

describe.skipIf(!TEST_URL)("news search on Postgres", () => {
  let client: PgClient;
  let repo: NewsRepository;
  let url: string;
  const vectors = unitVectors(60, 7);
  const [query] = unitVectors(1, 99);

  beforeAll(async () => {
    const previous = process.env.DATABASE_URL;
    url = await freshDatabase();
    process.env.DATABASE_URL = url;
    client = createPgClient();
    process.env.DATABASE_URL = previous;
    await client.ensureReady();
    await client.db.insert(newsItems).values(
      vectors.map((embedding, i) => ({
        source: i % 2 === 0 ? "habr" : "hackernews",
        externalId: `item-${i}`,
        title: `item ${i}`,
        body: `body ${i}`,
        embedding,
        embeddedAt: new Date(),
      })),
    );
    const embeddings: EmbeddingService = {
      embed: async () => query!,
      embedBatch: async (texts) => texts.map(() => query!),
    };
    repo = createNewsRepository({
      db: client.db,
      embeddings,
      embedder: createNewsEmbedder({ db: client.db, embeddings }),
    });
  });

  afterAll(async () => {
    await client?.close();
  });

  it("returns the nearest items in order, with the exact full-vector distance", async () => {
    const results = await repo.search({ query: "anything", k: 5, dedupThreshold: 0 });
    const expected = vectors
      .map((v, i) => ({ title: `item ${i}`, distance: cosineDistance(v, query!) }))
      .sort((a, b) => a.distance - b.distance)
      .slice(0, 5);
    expect(results.map((r) => r.title)).toEqual(expected.map((e) => e.title));
    results.forEach((r, i) => expect(r.distance).toBeCloseTo(expected[i]!.distance, 6));
  });

  it("returns a source's exact nearest items under a source filter", async () => {
    const results = await repo.search({ query: "anything", k: 10, dedupThreshold: 0, filter: { source: "habr" } });
    const expected = vectors
      .map((v, i) => ({ title: `item ${i}`, habr: i % 2 === 0, distance: cosineDistance(v, query!) }))
      .filter((e) => e.habr)
      .sort((a, b) => a.distance - b.distance)
      .slice(0, 10);
    expect(results.map((r) => r.title)).toEqual(expected.map((e) => e.title));
  });

  it("orders the pool by the expression the HNSW index covers", async () => {
    const { sql: text, params } = searchPool(client.db, query!, [], 30, "approximate").toSQL();
    const conn = new pg.Client({ connectionString: url });
    await conn.connect();
    try {
      // With the sequential scan priced out, a plan without the index means
      // the ORDER BY no longer matches the indexed expression.
      await conn.query("SET enable_seqscan = off");
      const plan = await conn.query(`EXPLAIN ${text}`, params);
      expect(plan.rows.map((r) => r["QUERY PLAN"]).join("\n")).toContain("news_items_embedding_hnsw");
    } finally {
      await conn.end();
    }
  });
});
