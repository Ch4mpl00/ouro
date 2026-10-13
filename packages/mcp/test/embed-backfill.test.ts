// `pnpm embed:backfill` — the repair path after an embedder outage — over
// the three stores it drains (news, knowledge notes, the memory index), on
// Postgres. Rows land while the embedder is down, so their vectors are NULL;
// then the backfill runs against an embedder that is up but rejects some
// inputs for good (like OpenAI rejecting one input fails the whole request)
// and now and then has another outage.
//
// Rules, from what was written and what the embedder accepts:
//   - after a backfill with the embedder up, every row whose text it accepts
//     has a vector — a row it never accepts stays NULL and holds up nothing,
//     wherever it sits in the backlog;
//   - a vector is the embedding of that row's own text;
//   - a backfill during an outage changes nothing and ends;
//   - running it again when nothing can be embedded changes nothing.

import fc from "fast-check";
import pg from "pg";
import { afterAll, beforeAll, describe, expect, it } from "vitest";
import { createPgClient, type PgClient } from "../src/db/pg/client";
import { drainMissing } from "../src/services/embeddings/backfill";
import type { EmbeddingProvider } from "../src/services/embeddings/provider";
import { createKnowledgeModule } from "../src/services/knowledge";
import { createMemoryModule } from "../src/services/memory";
import { createNewsModule } from "../src/services/news";
import { wordEmbedder } from "../specs/support/environment";
import { databaseAvailable, testDatabase, type TestDatabase } from "./support/database";

const POISON = "☠";

// An embedder that can be down, and that rejects any request containing a
// poisoned input — one bad input fails the whole request, as OpenAI's does.
function controlledEmbedder() {
  const state = { down: false, calls: 0 };
  const check = (texts: string[]) => {
    state.calls++;
    if (state.down) throw new Error("embedder unreachable");
    if (texts.some((t) => t.includes(POISON))) throw new Error("400 invalid input");
  };
  const provider: EmbeddingProvider = {
    dimensions: wordEmbedder.dimensions,
    embed: async (text) => {
      check([text]);
      return wordEmbedder.embed(text);
    },
    embedBatch: async (texts) => {
      check(texts);
      return wordEmbedder.embedBatch(texts);
    },
  };
  return { provider, state };
}

type Store = "news" | "note" | "fact";
interface Row {
  store: Store;
  poison: boolean;
}

const row = fc.record({
  store: fc.constantFrom<Store>("news", "note", "fact"),
  poison: fc.oneof({ weight: 4, arbitrary: fc.constant(false) }, { weight: 1, arbitrary: fc.constant(true) }),
});

interface Vectors {
  // `${table}:${text}` → has a vector, and whether it is that text's own.
  rows: { key: string; text: string; embedded: boolean; own: boolean }[];
}

const available = await databaseAvailable();

describe.skipIf(!available)("embed:backfill", () => {
  let db: TestDatabase;
  let client: PgClient;
  let pool: pg.Pool;

  beforeAll(async () => {
    db = await testDatabase();
    process.env.DATABASE_URL = db.url;
    client = createPgClient();
    await client.ensureReady();
    pool = new pg.Pool({ connectionString: db.url });
  }, 180_000);

  afterAll(async () => {
    await pool?.end();
    await client?.close();
    await db?.drop();
  });

  // Every embedded row, with whether its vector is the embedding of the text
  // it was embedded from (news: title + body; the others: their text).
  async function observe(): Promise<Vectors> {
    const out: Vectors["rows"] = [];
    const add = async (sql: string, table: string) => {
      for (const r of (await pool.query<{ text: string; embedding: string | null }>(sql)).rows) {
        const want = JSON.stringify((await wordEmbedder.embed(r.text)).map((x) => Math.round(x * 1e4)));
        const got = r.embedding === null ? null : JSON.stringify((JSON.parse(r.embedding) as number[]).map((x) => Math.round(x * 1e4)));
        out.push({ key: `${table}:${r.text}`, text: r.text, embedded: got !== null, own: got === want });
      }
    };
    await add("SELECT coalesce(title || E'\\n\\n', '') || body AS text, embedding::text FROM news_items", "news");
    await add("SELECT body AS text, embedding::text FROM knowledge_base_notes", "note");
    await add("SELECT text, embedding::text FROM memory_index", "index");
    return { rows: out.sort((a, b) => a.key.localeCompare(b.key)) };
  }

  it("embeds every row the embedder accepts, whatever rejected rows sit in the backlog", async () => {
    const witnessed = new Set<string>();
    await fc.assert(
      fc.asyncProperty(
        fc.array(row, { minLength: 1, maxLength: 25 }),
        fc.integer({ min: 1, max: 6 }),
        fc.boolean(),
        fc.array(fc.nat(), { maxLength: 8 }),
        async (rows, batchSize, outageFirst, touched) => {
          await pool.query("TRUNCATE news_items, knowledge_base_notes, memory_facts, memory_index RESTART IDENTITY");
          const { provider, state } = controlledEmbedder();
          const news = createNewsModule({ db: client.db, embeddingProvider: provider }).repository;
          const knowledge = createKnowledgeModule({ db: client.db, embeddingProvider: provider }).repository;
          const memory = createMemoryModule({ db: client.db, embeddingProvider: provider });

          // Everything lands during an outage.
          state.down = true;
          for (const [i, r] of rows.entries()) {
            const text = `entry ${i} ${r.store}${r.poison ? ` ${POISON}` : ""}`;
            if (r.store === "news") {
              await news.save([{ source: "hn", externalId: `e${i}`, title: null, url: null, body: text, metadata: {}, postedAt: null }]);
            } else if (r.store === "note") {
              await knowledge.addNote({ body: text });
            } else {
              await memory.service.remember({ body: text });
            }
          }
          // Rows updated since they landed (a re-fetched article, a re-tagged
          // note) sit elsewhere on disk than their id suggests: a no-op
          // update moves a row to the end of the table.
          for (const pick of touched) {
            for (const table of ["news_items", "knowledge_base_notes", "memory_index"]) {
              await pool.query(`UPDATE ${table} SET id = id WHERE id = $1`, [(pick % rows.length) + 1]);
            }
          }
          const backfill = () =>
            Promise.all([
              drainMissing("news", news.embedMissingBatch, { batchSize, log: () => {} }),
              drainMissing("knowledge", knowledge.embedMissingBatch, { batchSize, log: () => {} }),
              drainMissing("memory", memory.indexer.embedMissingBatch, { batchSize, log: () => {} }),
            ]);
          const landed = await observe();
          expect(landed.rows.every((r) => !r.embedded), "nothing embedded during the outage").toBe(true);

          if (outageFirst) {
            await backfill();
            expect(await observe(), "a backfill during an outage changes nothing").toEqual(landed);
          }

          state.down = false;
          await backfill();
          const after = await observe();
          const where = `${JSON.stringify(rows)} batch=${batchSize}`;
          for (const r of after.rows) {
            const poisoned = r.text.includes(POISON);
            expect(r.embedded, `${where}: ${r.key} ${poisoned ? "is rejected, so stays NULL" : "is accepted, so is embedded"}`).toBe(!poisoned);
            if (r.embedded) expect(r.own, `${where}: ${r.key} holds its own text's vector`).toBe(true);
          }
          if (rows.some((r) => r.poison) && rows.some((r) => !r.poison)) witnessed.add("accepted and rejected rows in one backlog");

          const calls = state.calls;
          await backfill();
          expect(await observe(), `${where}: running again changes nothing`).toEqual(after);
          if (!rows.some((r) => r.poison)) expect(state.calls, `${where}: nothing left, nothing asked`).toBe(calls);
        },
      ),
      { numRuns: Number(process.env.PROPERTY_RUNS ?? 40) },
    );
    expect([...witnessed]).toEqual(["accepted and rejected rows in one backlog"]);
  }, 600_000);
});
