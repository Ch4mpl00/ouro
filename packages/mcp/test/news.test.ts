// The news store as the agent reads it: list_news and search_news over MCP
// against Postgres, on a generated corpus (sources, channels, dates, bodies
// that are often near-duplicates of each other, some rows whose embedding
// failed). Expectations are relations over the stored rows — filters,
// ordering, distances recomputed from the stored vectors, no near-duplicate
// pairs, and nothing better left out — not a re-run of the ranking code.
//
// PROPERTY_RUNS raises the number of generated corpora.

import type { Client } from "@modelcontextprotocol/sdk/client/index.js";
import fc from "fast-check";
import pg from "pg";
import { afterAll, beforeAll, describe, expect, it } from "vitest";
import { z } from "zod";
import { createNewsModule, type NewsRepository } from "../src/services/news";
import { call, connect, startServer, wordEmbedder, type Outcome, type Server } from "../specs/support/environment";
import { databaseAvailable, testDatabase, type TestDatabase } from "./support/database";

const RUNS = Number(process.env.PROPERTY_RUNS ?? 30);
const DEDUP = 0.03; // services/retrieval/dedup.ts DEFAULT_DEDUP_THRESHOLD
const EPS = 1e-6;

// ── observation ──────────────────────────────────────────────────────────────

interface Row {
  id: number;
  source: string;
  externalId: string;
  title: string | null;
  body: string;
  channel: string | null;
  postedAt: number | null;
  fetchedAt: number;
  embeddedAt: number | null;
  embedding: number[] | null;
}

const rowSchema = z.object({
  id: z.number().int(),
  source: z.string(),
  external_id: z.string(),
  title: z.string().nullable(),
  body: z.string(),
  channel: z.string().nullable(),
  posted_ms: z.number().nullable(),
  fetched_ms: z.number(),
  embedded_ms: z.number().nullable(),
  embedding: z.string().nullable(),
});

async function observe(pool: pg.Pool): Promise<Row[]> {
  const { rows } = await pool.query(
    `SELECT id::int AS id, source, external_id, title, body, metadata ->> 'chat_username' AS channel,
            (extract(epoch FROM posted_at) * 1000)::float8 AS posted_ms,
            (extract(epoch FROM fetched_at) * 1000)::float8 AS fetched_ms,
            (extract(epoch FROM embedded_at) * 1000)::float8 AS embedded_ms,
            embedding::text AS embedding
       FROM news_items`,
  );
  return rows.map((raw) => {
    const r = rowSchema.parse(raw);
    return {
      id: r.id,
      source: r.source,
      externalId: r.external_id,
      title: r.title,
      body: r.body,
      channel: r.channel,
      postedAt: r.posted_ms,
      fetchedAt: r.fetched_ms,
      embeddedAt: r.embedded_ms,
      embedding: r.embedding === null ? null : z.array(z.number()).parse(JSON.parse(r.embedding)),
    };
  });
}

function cosineDistance(a: number[], b: number[]): number {
  let dot = 0;
  let na = 0;
  let nb = 0;
  for (let i = 0; i < a.length; i++) {
    dot += a[i]! * b[i]!;
    na += a[i]! * a[i]!;
    nb += b[i]! * b[i]!;
  }
  return 1 - dot / (Math.sqrt(na) * Math.sqrt(nb));
}

// ── corpus and queries ───────────────────────────────────────────────────────

const WORDS = ["rust", "postgres", "llm", "agents", "kotlin", "tax", "ukraine", "startup", "gpu", "privacy", "ai", "compiler"];
const sentence = fc.array(fc.constantFrom(...WORDS), { minLength: 1, maxLength: 5 }).map((ws) => ws.join(" "));
const DAY = 24 * 3600_000;
const T0 = Date.parse("2026-10-01T00:00:00Z");
const SOURCES = ["hackernews", "habr", "channel"] as const;
const CHANNELS = ["fop_news", "devua"];

const item = fc.record({
  source: fc.constantFrom(...SOURCES),
  externalId: fc.integer({ min: 1, max: 40 }).map(String),
  // Mostly untitled, short bodies: plenty of near-duplicates to dedup.
  title: fc.oneof({ weight: 3, arbitrary: fc.constant(null) }, { weight: 1, arbitrary: sentence }),
  body: fc.oneof(sentence, fc.constantFrom(...WORDS), fc.constantFrom(...WORDS).map((w) => `${w} ${w}`)),
  channel: fc.constantFrom(...CHANNELS),
  postedAt: fc.oneof({ weight: 1, arbitrary: fc.constant(null) }, { weight: 7, arbitrary: fc.integer({ min: 0, max: 10 * DAY }).map((d) => T0 + d) }),
  embeds: fc.oneof({ weight: 5, arbitrary: fc.constant(true) }, { weight: 1, arbitrary: fc.constant(false) }),
});

type Item = typeof item extends fc.Arbitrary<infer T> ? T : never;

const instant = fc.integer({ min: -DAY, max: 11 * DAY }).map((d) => new Date(T0 + d).toISOString());
// Each filter present about a quarter of the time, so most queries still
// have candidates. (fc.option's `freq` is 1/P(nil), the other way round.)
const sometimes = <T>(arb: fc.Arbitrary<T>) =>
  fc.oneof({ weight: 3, arbitrary: fc.constant(undefined) }, { weight: 1, arbitrary: arb });
const filters = fc.record({
  source: sometimes(fc.constantFrom(...SOURCES)),
  sinceISO: sometimes(instant),
  untilISO: sometimes(instant),
  channel: sometimes(fc.constantFrom(...CHANNELS)),
});

type Filters = { source?: string; sinceISO?: string; untilISO?: string; channel?: string };

// The channel filter only means something for channel posts; on other
// sources it is about chat_username, which they don't have.
function matches(r: Row, f: Filters): boolean {
  if (f.source !== undefined && r.source !== f.source) return false;
  if (f.sinceISO !== undefined && !(r.postedAt !== null && r.postedAt > Date.parse(f.sinceISO))) return false;
  if (f.untilISO !== undefined && !(r.postedAt !== null && r.postedAt <= Date.parse(f.untilISO))) return false;
  if (f.channel !== undefined && r.channel !== f.channel) return false;
  return true;
}

const listReply = z.object({
  count: z.number().int(),
  items: z.array(z.object({ source: z.string(), externalId: z.string() }).passthrough()),
});
const searchItem = z.object({ id: z.number().int(), source: z.string(), distance: z.number(), matchedQueries: z.array(z.number().int()).optional() }).passthrough();
const searchReply = z.object({ count: z.number().int(), results: z.array(searchItem) });
const chunkReply = z.object({ count: z.number().int(), chunks: z.array(z.array(searchItem)) });

function value(o: Outcome, op: string): unknown {
  if (o.kind !== "ok") throw new Error(`${op}: ${JSON.stringify(o)}`);
  return o.value;
}

// ── suite ────────────────────────────────────────────────────────────────────

const available = await databaseAvailable();

describe.skipIf(!available)("news store over MCP against Postgres", () => {
  let db: TestDatabase;
  let server: Server;
  let client: Client;
  let pool: pg.Pool;
  let repo: NewsRepository;

  beforeAll(async () => {
    db = await testDatabase();
    server = await startServer(db.url, ["news-read"]);
    client = await connect(server.url);
    pool = new pg.Pool({ connectionString: db.url });
    repo = createNewsModule({ db: server.pg.db, embeddingProvider: wordEmbedder }).repository;
  }, 180_000);

  afterAll(async () => {
    await pool?.end();
    await client?.close();
    await server?.stop();
    await db?.drop();
  });

  async function seed(items: Item[]): Promise<Row[]> {
    await pool.query("TRUNCATE news_items RESTART IDENTITY");
    await repo.save(
      items.map((i) => ({
        source: i.source,
        externalId: i.externalId,
        title: i.title,
        url: null,
        body: i.body,
        metadata: i.source === "channel" ? { chat_username: i.channel } : {},
        postedAt: i.postedAt === null ? null : new Date(i.postedAt),
      })),
    );
    // The embedder was down for some of them.
    const unembedded = [...new Set(items.filter((i) => !i.embeds).map((i) => `${i.source}:${i.externalId}`))];
    await pool.query("UPDATE news_items SET embedding = NULL, embedded_at = NULL WHERE source || ':' || external_id = ANY($1)", [unembedded]);
    return observe(pool);
  }

  it("save keeps the first copy of each (source, externalId) and embeds what it stored", async () => {
    await fc.assert(
      fc.asyncProperty(fc.array(item, { minLength: 1, maxLength: 30, size: "max" }), async (items) => {
        const rows = await seed(items.map((i) => ({ ...i, embeds: true })));
        const firsts = new Map<string, (typeof items)[number]>();
        for (const i of items) if (!firsts.has(`${i.source}:${i.externalId}`)) firsts.set(`${i.source}:${i.externalId}`, i);
        expect(rows.map((r) => `${r.source}:${r.externalId}`).sort()).toEqual([...firsts.keys()].sort());
        for (const r of rows) {
          expect(r.body, "the first copy wins").toBe(firsts.get(`${r.source}:${r.externalId}`)?.body);
          expect(r.embedding, "embedded on save").not.toBeNull();
        }
        // Saving the same batch again changes nothing.
        const again = await repo.save(items.map((i) => ({ source: i.source, externalId: i.externalId, title: i.title, url: null, body: `${i.body} changed`, metadata: {}, postedAt: null })));
        expect(again.saved).toBe(0);
        expect(await observe(pool)).toEqual(rows);
      }),
      { numRuns: RUNS },
    );
  }, 300_000);

  it("list_news returns matching rows in posted order, without near-duplicates, leaving nothing out", async () => {
    await fc.assert(
      fc.asyncProperty(fc.array(item, { minLength: 1, maxLength: 40, size: "max" }), filters, fc.option(fc.integer({ min: 1, max: 12 }), { nil: undefined }), async (items, f, limit) => {
        const rows = await seed(items);
        const args: Record<string, unknown> = { ...f };
        if (limit !== undefined) args.limit = limit;
        const reply = listReply.parse(value(await call(client, "list_news", args), "list_news"));
        const cap = limit ?? 500;
        const got = reply.items.map((it) => {
          const r = rows.find((x) => x.source === it.source && x.externalId === it.externalId);
          if (!r) throw new Error(`list_news returned a row not in the store: ${JSON.stringify(it)}`);
          return r;
        });
        const where = `${JSON.stringify(f)} limit=${limit}`;
        expect(reply.count, where).toBe(got.length);
        expect(got.length, `${where}: at most limit`).toBeLessThanOrEqual(cap);
        expect(new Set(got.map((r) => r.id)).size, `${where}: no row twice`).toBe(got.length);
        for (const r of got) expect(matches(r, f), `${where}: ${r.source}:${r.externalId} passes the filters`).toBe(true);
        // Order by posted_at: ascending with sinceISO, descending otherwise
        // (undated rows have no place in it — F11).
        const dated = got.filter((r) => r.postedAt !== null).map((r) => r.postedAt!);
        const sorted = [...dated].sort((a, b) => (f.sinceISO ? a - b : b - a));
        expect(dated, `${where}: posted order`).toEqual(sorted);
        const vecs = got.filter((r) => r.embedding).map((r) => r.embedding!);
        for (let i = 0; i < vecs.length; i++) {
          for (let j = i + 1; j < vecs.length; j++) {
            expect(cosineDistance(vecs[i]!, vecs[j]!), `${where}: two near-duplicates listed`).toBeGreaterThanOrEqual(DEDUP - EPS);
          }
        }
        // Nothing left out: a matching row is missing only if the list is
        // full, or it duplicates a listed row.
        if (got.length < cap) {
          for (const r of rows.filter((x) => matches(x, f))) {
            if (got.includes(r)) continue;
            const dup = r.embedding !== null && got.some((g) => g.embedding !== null && cosineDistance(g.embedding, r.embedding!) < DEDUP);
            expect(dup, `${where}: ${r.source}:${r.externalId} left out of a short list`).toBe(true);
          }
        }
      }),
      { numRuns: RUNS },
    );
  }, 300_000);

  it("search_news returns the closest matching rows, best first, without near-duplicates", async () => {
    await fc.assert(
      fc.asyncProperty(
        fc.array(item, { minLength: 1, maxLength: 40, size: "max" }),
        filters,
        fc.oneof(sentence.map((q) => [q]), fc.array(sentence, { minLength: 2, maxLength: 3 })),
        fc.integer({ min: 1, max: 12 }),
        fc.option(fc.integer({ min: 2, max: 5 }), { nil: undefined }),
        async (items, f, queries, k, chunks) => {
          const rows = await seed(items);
          const args: Record<string, unknown> = { ...f, k, ...(queries.length === 1 ? { query: queries[0] } : { queries }) };
          const flat = searchReply.parse(value(await call(client, "search_news", args), "search_news"));
          const where = `${JSON.stringify({ f, queries, k })}`;
          const qv = await Promise.all(queries.map((q) => wordEmbedder.embed(q)));
          const candidates = rows.filter((r) => r.embedding !== null && matches(r, f));
          const distanceOf = (r: Row) => Math.min(...qv.map((v) => cosineDistance(v, r.embedding!)));

          const got = flat.results.map((res) => {
            const r = rows.find((x) => x.id === res.id);
            if (!r) throw new Error(`search_news returned an id not in the store: ${res.id}`);
            return { r, res };
          });
          expect(flat.count, where).toBe(got.length);
          expect(got.length, `${where}: at most k`).toBeLessThanOrEqual(k);
          for (const { r, res } of got) {
            expect(r.embedding, `${where}: only searchable rows`).not.toBeNull();
            expect(matches(r, f), `${where}: ${r.id} passes the filters`).toBe(true);
            expect(Math.abs(res.distance - distanceOf(r)), `${where}: ${r.id}'s distance is its best over the queries`).toBeLessThan(1e-4);
            if (queries.length > 1) {
              expect(res.matchedQueries?.length, `${where}: matchedQueries names at least one query`).toBeGreaterThan(0);
            }
          }
          const ds = got.map((g) => g.res.distance);
          expect(ds, `${where}: best first`).toEqual([...ds].sort((a, b) => a - b));
          for (let i = 0; i < got.length; i++) {
            for (let j = i + 1; j < got.length; j++) {
              expect(cosineDistance(got[i]!.r.embedding!, got[j]!.r.embedding!), `${where}: near-duplicates returned`).toBeGreaterThanOrEqual(DEDUP - EPS);
            }
          }
          // Nothing better left out: a missing candidate is no closer than
          // the last result (when k are back), duplicates a returned row, or
          // lies beyond each query's candidate pool.
          const poolSize = Math.max(k * 2, 30);
          const rankOf = (r: Row) => Math.min(...qv.map((v) => candidates.filter((c) => cosineDistance(v, c.embedding!) < cosineDistance(v, r.embedding!) - EPS).length));
          const worst = ds.at(-1) ?? Infinity;
          for (const c of candidates) {
            if (got.some((g) => g.r.id === c.id)) continue;
            const dc = distanceOf(c);
            const fullAndNoCloser = got.length === k && dc >= worst - 1e-4;
            const dup = got.some((g) => cosineDistance(g.r.embedding!, c.embedding!) < DEDUP);
            const outsidePool = rankOf(c) >= poolSize;
            expect(fullAndNoCloser || dup || outsidePool, `${where}: ${c.id} (distance ${dc}) left out`).toBe(true);
          }
          if (candidates.length > 0) expect(got.length, `${where}: something matches, so something comes back`).toBeGreaterThan(0);

          if (chunks !== undefined) {
            const split = chunkReply.parse(value(await call(client, "search_news", { ...args, chunks }), "search_news chunks"));
            expect(split.chunks, `${where}: exactly n chunks`).toHaveLength(chunks);
            expect(split.chunks.flat().map((x) => x.id), `${where}: chunks are the results, in order`).toEqual(flat.results.map((x) => x.id));
            const sizes = split.chunks.map((c) => c.length);
            expect(Math.max(...sizes) - Math.min(...sizes), `${where}: balanced`).toBeLessThanOrEqual(1);
            expect(sizes, `${where}: trailing chunks are the short ones`).toEqual([...sizes].sort((a, b) => b - a));
          }
        },
      ),
      { numRuns: RUNS },
    );
  }, 300_000);

  // Near-duplicates at the top must not crowd out the next distinct match:
  // k results come back as long as k distinct ones exist.
  it("dedup at the top leaves room for the next distinct match", async () => {
    await seed([
      { source: "habr", externalId: "1", title: null, body: "rust", channel: "devua", postedAt: T0, embeds: true },
      { source: "habr", externalId: "2", title: null, body: "rust rust", channel: "devua", postedAt: T0, embeds: true },
      { source: "habr", externalId: "3", title: null, body: "rust tax", channel: "devua", postedAt: T0, embeds: true },
    ]);
    const reply = searchReply.parse(value(await call(client, "search_news", { query: "rust", k: 2 }), "search_news"));
    expect(reply.results).toHaveLength(2);
  });

  it("an invalid date filter is refused, not a crash", async () => {
    for (const tool of ["list_news", "search_news"]) {
      const outcome = await call(client, tool, { query: "rust", sinceISO: "yesterday" });
      expect(outcome.kind === "ok" && typeof outcome.value === "object" && outcome.value !== null && "error" in outcome.value ? "error" : outcome.kind, tool).not.toBe("ok");
    }
  });
});
