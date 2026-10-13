// fetch_article over MCP on a real server and Postgres, with the web played
// by a stand-in fetcher that serves articles, binaries, errors and bodies
// far bigger than any article — counting every byte the server pulls. The
// URL comes from the model (or a ChatGPT user over the tunnel); the HN and
// Habr pollers run the same extraction on whatever the front page links to.
//
// Rules, from the page served:
//   - no response is read past a bound, whatever its size: one huge link
//     must not take the server's memory with it;
//   - a binary (PDF, image, archive, video) is refused without being read,
//     and nothing is stored;
//   - an error status is a tool error, and nothing is stored;
//   - an HTML article comes back as its text, is stored once and becomes
//     searchable; asking again answers from the store without fetching.

import fc from "fast-check";
import pg from "pg";
import { Response } from "undici";
import { afterAll, beforeAll, describe, expect, it } from "vitest";
import type { GuardedFetch } from "../src/services/fetch";
import { call, startServer, type Server } from "../specs/support/environment";
import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { StreamableHTTPClientTransport } from "@modelcontextprotocol/sdk/client/streamableHttp.js";
import { databaseAvailable, testDatabase, type TestDatabase } from "./support/database";

const CHUNK = 64 * 1024;
// What any page may cost: the article cap plus one chunk of read-ahead.
const READ_BOUND = 8 * 1024 * 1024 + CHUNK;

type Page =
  | { kind: "article"; words: string; padKb: number; contentType: string | null }
  | { kind: "binary"; contentType: string; sizeKb: number }
  | { kind: "error"; status: number }
  | { kind: "endless"; contentType: string };

const WORDS = ["kestrel", "basalt", "quokka", "fjord", "zephyr", "lichen", "osprey", "tundra"];
const page: fc.Arbitrary<Page> = fc.oneof(
  {
    weight: 4,
    arbitrary: fc.record({
      kind: fc.constant("article" as const),
      words: fc.shuffledSubarray(WORDS, { minLength: 3, maxLength: 3 }).map((ws) => ws.join(" ")),
      padKb: fc.oneof({ weight: 4, arbitrary: fc.integer({ min: 0, max: 50 }) }, { weight: 1, arbitrary: fc.integer({ min: 9_000, max: 20_000 }) }),
      contentType: fc.constantFrom("text/html; charset=utf-8", "text/html", "application/xhtml+xml", null),
    }),
  },
  {
    weight: 2,
    arbitrary: fc.record({
      kind: fc.constant("binary" as const),
      contentType: fc.constantFrom("application/pdf", "image/png", "application/zip", "video/mp4", "application/octet-stream"),
      sizeKb: fc.integer({ min: 1, max: 30_000 }),
    }),
  },
  { weight: 1, arbitrary: fc.record({ kind: fc.constant("error" as const), status: fc.constantFrom(404, 410, 500, 503) }) },
  { weight: 1, arbitrary: fc.record({ kind: fc.constant("endless" as const), contentType: fc.constantFrom("text/html", "application/octet-stream") }) },
);

const paragraph = (words: string, i: number) =>
  `<p>Paragraph ${i} about the ${words}. This sentence is long enough for a reader to count it as prose, and it keeps going for a while so that Readability sees a real article body here.</p>`;

// A body streamed in chunks, counting what the reader pulls.
function streamed(chunks: () => Generator<Uint8Array>, pulled: { bytes: number }): ReadableStream<Uint8Array> {
  const it = chunks();
  return new ReadableStream({
    pull(controller) {
      const next = it.next();
      if (next.done) return controller.close();
      pulled.bytes += next.value.byteLength;
      controller.enqueue(next.value);
    },
  });
}

function* inChunks(text: string): Generator<Uint8Array> {
  const bytes = new TextEncoder().encode(text);
  for (let i = 0; i < bytes.length; i += CHUNK) yield bytes.subarray(i, i + CHUNK);
}

const available = await databaseAvailable();

describe.skipIf(!available)("fetch_article", () => {
  let db: TestDatabase;
  let server: Server;
  let client: Client;
  let pool: pg.Pool;
  const pages = new Map<string, Page>();
  const pulled = new Map<string, { bytes: number }>();
  const hits = new Map<string, number>();

  const fetcher: GuardedFetch = async (url) => {
    hits.set(url, (hits.get(url) ?? 0) + 1);
    const p = pages.get(url);
    const count = { bytes: 0 };
    pulled.set(url, count);
    if (!p) return { url, response: new Response("not found", { status: 404 }) };
    if (p.kind === "error") return { url, response: new Response("gone", { status: p.status }) };
    if (p.kind === "binary") {
      const bytes = p.sizeKb * 1024;
      const body = streamed(function* () {
        for (let sent = 0; sent < bytes; sent += CHUNK) yield new Uint8Array(Math.min(CHUNK, bytes - sent));
      }, count);
      return { url, response: new Response(body, { headers: { "content-type": p.contentType } }) };
    }
    if (p.kind === "endless") {
      const body = streamed(function* () {
        const chunk = new TextEncoder().encode(paragraph("endless", 0).repeat(Math.ceil(CHUNK / 200))).subarray(0, CHUNK);
        for (;;) yield chunk;
      }, count);
      return { url, response: new Response(body, { headers: { "content-type": p.contentType } }) };
    }
    const html = (padKb: number) => {
      const head = `<!doctype html><html><head><title>On the ${p.words}</title></head><body><article><h1>On the ${p.words}</h1>`;
      const paras = Array.from({ length: 6 }, (_, i) => paragraph(p.words, i)).join("");
      const pad = `<p>${"filler text ".repeat(Math.ceil((padKb * 1024) / 12))}</p>`;
      return `${head}${paras}</article><aside>${pad}</aside></body></html>`;
    };
    const body = streamed(() => inChunks(html(p.padKb)), count);
    return { url, response: new Response(body, { headers: p.contentType ? { "content-type": p.contentType } : {} }) };
  };

  beforeAll(async () => {
    db = await testDatabase();
    server = await startServer(db.url, undefined, fetcher);
    client = new Client({ name: "t", version: "0" });
    await client.connect(new StreamableHTTPClientTransport(new URL(server.url)));
    pool = new pg.Pool({ connectionString: db.url });
  }, 180_000);

  afterAll(async () => {
    await client?.close();
    await pool?.end();
    await server?.stop();
    await db?.drop();
  });

  it("bounds what it reads, refuses binaries, and stores each article once", async () => {
    const witnessed = new Set<string>();
    let n = 0;
    await fc.assert(
      fc.asyncProperty(page, fc.boolean(), async (p, again) => {
        const url = `https://example.org/a/${++n}`;
        // A word of its own, so search can tell it from another article
        // drawn with the same words.
        if (p.kind === "article") p.words = `${p.words} ref${n}`;
        pages.set(url, p);
        const before = Number((await pool.query("SELECT count(*) FROM news_items")).rows[0].count);
        const out = await call(client, "fetch_article", { url });
        const read = pulled.get(url)?.bytes ?? 0;
        const stored = (await pool.query("SELECT body FROM news_items WHERE external_id = $1", [url])).rows;
        const where = `${url} ${JSON.stringify(p)}: ${JSON.stringify(out).slice(0, 200)}`;

        expect(read, `${where}: read ${read} bytes`).toBeLessThanOrEqual(READ_BOUND);
        if (p.kind === "endless" || (p.kind === "binary" && p.sizeKb * 1024 > READ_BOUND) || (p.kind === "article" && p.padKb * 1024 > READ_BOUND)) {
          witnessed.add("a body bigger than the bound");
        }
        if (p.kind === "binary" || (p.kind === "endless" && p.contentType !== "text/html")) {
          witnessed.add("a binary");
          expect(out.kind, `${where}: a binary is refused`).toBe("toolError");
          expect(read, `${where}: …without reading it`).toBeLessThanOrEqual(CHUNK);
          expect(stored, `${where}: nothing stored`).toEqual([]);
          return;
        }
        if (p.kind === "error") {
          expect(out.kind, `${where}: an error status is a tool error`).toBe("toolError");
          expect(stored, `${where}: nothing stored`).toEqual([]);
          return;
        }
        if (p.kind === "endless") return; // HTML that never ends: bounded is all we ask
        witnessed.add("an article");
        expect(out.kind, where).toBe("ok");
        const text = String(Reflect.get(Object(out.kind === "ok" ? out.value : {}), "text"));
        expect(text, `${where}: the article's own text`).toContain(`Paragraph 0 about the ${p.words}`);
        expect(stored.length, `${where}: stored once`).toBe(1);
        expect(Number((await pool.query("SELECT count(*) FROM news_items")).rows[0].count), where).toBe(before + 1);
        const found = await call(client, "search_news", { query: p.words, limit: 50 });
        expect(JSON.stringify(found), `${where}: searchable`).toContain(url);
        if (again) {
          const fetches = hits.get(url);
          const second = await call(client, "fetch_article", { url });
          expect(second, `${where}: answered from the store`).toMatchObject({ kind: "ok", value: { cached: true, text } });
          expect(hits.get(url), `${where}: without fetching again`).toBe(fetches);
        }
      }),
      { numRuns: Number(process.env.PROPERTY_RUNS ?? 40) },
    );
    expect([...witnessed].sort()).toEqual(["a binary", "a body bigger than the bound", "an article"]);
  }, 600_000);
});
