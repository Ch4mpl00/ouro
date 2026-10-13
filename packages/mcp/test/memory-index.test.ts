// Unified memory: facts and documents against their search projection
// (memory_index), on a real Postgres.
//
// 1. Over MCP: generated remember / update_fact / get_fact / recall
//    sequences; after every step the index mirrors every fact exactly (text,
//    state, tags), and recall only ever hands out refs that resolve.
// 2. Through the memory module with an embedder of random latency: writes
//    racing on one fact or one document leave the index describing the
//    last write, not whichever write finished embedding last.
//
// PROPERTY_RUNS raises the number of generated sequences.

import type { Client } from "@modelcontextprotocol/sdk/client/index.js";
import fc from "fast-check";
import pg from "pg";
import { afterAll, beforeAll, beforeEach, describe, expect, it } from "vitest";
import { z } from "zod";
import type { EmbeddingProvider } from "../src/services/embeddings/provider";
import { createMemoryModule, type MemoryModule } from "../src/services/memory";
import { call, connect, startServer, type Outcome, type Server } from "../specs/support/environment";
import { databaseAvailable, testDatabase, type TestDatabase } from "./support/database";

const RUNS = Number(process.env.PROPERTY_RUNS ?? 30);

// ── observation ──────────────────────────────────────────────────────────────

interface StoredFact {
  id: number;
  body: string;
  tags: string[];
  state: string;
}
interface IndexRow {
  sourceRef: string;
  ref: string;
  text: string;
  tags: string[];
  state: string;
}

const factRow = z.object({ id: z.number().int(), body: z.string(), tags: z.array(z.string()), state: z.string() });
const indexRow = z.object({ source_ref: z.string(), ref: z.string(), text: z.string(), tags: z.array(z.string()), state: z.string() });

async function observe(pool: pg.Pool): Promise<{ facts: Map<number, StoredFact>; index: IndexRow[] }> {
  const f = await pool.query("SELECT id::int AS id, body, tags, state FROM memory_facts ORDER BY id");
  const i = await pool.query("SELECT source_ref, ref, text, tags, state FROM memory_index ORDER BY source_ref, ref");
  return {
    facts: new Map(f.rows.map((raw) => {
      const r = factRow.parse(raw);
      return [r.id, r];
    })),
    index: i.rows.map((raw) => {
      const r = indexRow.parse(raw);
      return { sourceRef: r.source_ref, ref: r.ref, text: r.text, tags: r.tags, state: r.state };
    }),
  };
}

// The projection of a fact is one row carrying exactly its body, tags and
// state.
function indexMirrorsFacts(world: Awaited<ReturnType<typeof observe>>): string[] {
  const problems: string[] = [];
  for (const fact of world.facts.values()) {
    const rows = world.index.filter((r) => r.sourceRef === `fact:${fact.id}`);
    if (rows.length !== 1) {
      problems.push(`fact:${fact.id} has ${rows.length} index rows`);
      continue;
    }
    const [row] = rows;
    if (row!.ref !== `fact:${fact.id}` || row!.text !== fact.body || row!.state !== fact.state || JSON.stringify(row!.tags) !== JSON.stringify(fact.tags)) {
      problems.push(`fact:${fact.id} index ${JSON.stringify(row)} ≠ fact ${JSON.stringify(fact)}`);
    }
  }
  for (const row of world.index) {
    const m = /^fact:(\d+)$/.exec(row.sourceRef);
    if (m && !world.facts.has(Number(m[1]))) problems.push(`index row for a fact that doesn't exist: ${row.sourceRef}`);
  }
  return problems;
}

// ── replies ──────────────────────────────────────────────────────────────────

const fact = z.object({ id: z.number().int(), body: z.string(), tags: z.array(z.string()), state: z.string() }).passthrough();
const factReply = z.object({ ok: z.literal(true), fact }).passthrough();
const recallReply = z.object({ ok: z.literal(true), hits: z.array(z.object({ ref: z.string(), text: z.string() }).passthrough()) }).passthrough();

function value(outcome: Outcome, op: string): unknown {
  if (outcome.kind !== "ok") throw new Error(`${op}: expected a result, got ${JSON.stringify(outcome)}`);
  return outcome.value;
}
const refused = (o: Outcome) => o.kind !== "ok" || (typeof o.value === "object" && o.value !== null && "ok" in o.value && o.value.ok === false);

// ── operations ───────────────────────────────────────────────────────────────

const WORDS = ["router", "password", "internet", "bill", "alex", "tram", "lisbon", "pills", "monday", "garage"];
const sentence = fc.array(fc.constantFrom(...WORDS), { minLength: 2, maxLength: 6 }).map((ws) => ws.join(" "));
const body = fc.oneof(
  { weight: 6, arbitrary: sentence },
  { weight: 1, arbitrary: fc.constantFrom("  padded  body  ", "   ", "\n\t") },
);
const tags = fc.array(fc.constantFrom("home", "money", "travel", " home ", "health"), { maxLength: 4 });

type Op =
  | { kind: "remember"; body: string; tags: string[] | undefined }
  | { kind: "update"; pick: number; body: string | undefined; tags: string[] | undefined; state: "active" | "archived" | undefined }
  | { kind: "get"; pick: number }
  | { kind: "recall"; query: string | { exactOf: number }; tags: string[] | undefined; limit: number | undefined };

const op: fc.Arbitrary<Op> = fc.oneof(
  { weight: 3, arbitrary: fc.record({ kind: fc.constant("remember" as const), body, tags: fc.option(tags, { nil: undefined }) }) },
  {
    weight: 3,
    arbitrary: fc.record({
      kind: fc.constant("update" as const),
      pick: fc.nat(),
      // Blank more often than in `remember`, so a refused update shows up
      // in every run.
      body: fc.option(fc.oneof({ weight: 3, arbitrary: body }, { weight: 1, arbitrary: fc.constantFrom("   ", "\n\t") }), { nil: undefined }),
      tags: fc.option(tags, { nil: undefined }),
      state: fc.option(fc.constantFrom("active" as const, "archived" as const), { nil: undefined }),
    }),
  },
  { weight: 1, arbitrary: fc.record({ kind: fc.constant("get" as const), pick: fc.nat() }) },
  // Asking for an active fact word for word, no filters.
  {
    weight: 2,
    arbitrary: fc.nat().map((n): Op => ({ kind: "recall", query: { exactOf: n }, tags: undefined, limit: 50 })),
  },
  {
    weight: 3,
    arbitrary: fc.record({
      kind: fc.constant("recall" as const),
      query: fc.oneof(sentence, fc.nat().map((n) => ({ exactOf: n }))),
      tags: fc.option(fc.array(fc.constantFrom("home", "money", "travel"), { minLength: 1, maxLength: 2 }), { nil: undefined }),
      limit: fc.option(fc.integer({ min: 1, max: 50 }), { nil: undefined }),
    }),
  },
);

const trimmedTags = (t: string[]) => [...new Set(t.map((x) => x.trim()).filter((x) => x.length > 0))];

const available = await databaseAvailable();

describe.skipIf(!available)("memory facts and their search projection", () => {
  let db: TestDatabase;
  let server: Server;
  let client: Client;
  let pool: pg.Pool;

  beforeAll(async () => {
    db = await testDatabase();
    server = await startServer(db.url, ["memory"]);
    client = await connect(server.url);
    pool = new pg.Pool({ connectionString: db.url });
  }, 180_000);

  afterAll(async () => {
    await pool?.end();
    await client?.close();
    await server?.stop();
    await db?.drop();
  });

  beforeEach(async () => {
    await pool.query("TRUNCATE memory_facts, memory_index RESTART IDENTITY");
  });

  it("keeps the index a mirror of the facts and recall's refs resolvable", async () => {
    const witnessed = new Set<string>();
    await fc.assert(
      fc.asyncProperty(fc.array(op, { minLength: 1, maxLength: 20, size: "max" }), async (ops) => {
        await pool.query("TRUNCATE memory_facts, memory_index RESTART IDENTITY");
        for (const [i, o] of ops.entries()) {
          const before = await observe(pool);
          const ids = [...before.facts.keys()];
          const where = `step ${i} ${JSON.stringify(o)}`;

          if (o.kind === "remember") {
            const args: Record<string, unknown> = { body: o.body };
            if (o.tags) args.tags = o.tags;
            const outcome = await call(client, "remember", args);
            const after = await observe(pool);
            if (o.body.trim() === "") {
              expect(refused(outcome), `${where}: a blank fact is refused`).toBe(true);
              expect(after.facts.size, `${where}: nothing stored`).toBe(before.facts.size);
            } else {
              const got = factReply.parse(value(outcome, "remember")).fact;
              const stored = after.facts.get(got.id);
              expect(stored, `${where}: stored as returned`).toEqual({ id: got.id, body: o.body.trim(), tags: trimmedTags(o.tags ?? []), state: "active" });
            }
          } else if (o.kind === "update") {
            if (ids.length === 0) continue;
            const id = ids[o.pick % ids.length]!;
            const was = before.facts.get(id)!;
            const args: Record<string, unknown> = { id };
            if (o.body !== undefined) args.body = o.body;
            if (o.tags !== undefined) args.tags = o.tags;
            if (o.state !== undefined) args.state = o.state;
            const outcome = await call(client, "update_fact", args);
            const after = await observe(pool);
            if (o.body !== undefined && o.body.trim() === "") {
              witnessed.add("a blank update is refused");
              expect(refused(outcome), `${where}: a fact can't be blanked by an update either`).toBe(true);
              expect(after.facts.get(id), `${where}: a refused update changes nothing`).toEqual(was);
            } else {
              factReply.parse(value(outcome, "update_fact"));
              expect(after.facts.get(id), `${where}: exactly the given fields change`).toEqual({
                id,
                body: o.body === undefined ? was.body : o.body.trim(),
                tags: o.tags === undefined ? was.tags : trimmedTags(o.tags),
                state: o.state ?? was.state,
              });
              if (o.state === "archived") witnessed.add("a fact is archived");
            }
            for (const [other, f] of before.facts) if (other !== id) expect(after.facts.get(other), `${where}: other facts untouched`).toEqual(f);
          } else if (o.kind === "get") {
            if (ids.length === 0) continue;
            const id = ids[o.pick % ids.length]!;
            const got = factReply.parse(value(await call(client, "get_fact", { id }), "get_fact")).fact;
            expect({ id: got.id, body: got.body, tags: got.tags, state: got.state }, `${where}: get_fact returns the stored fact`).toEqual(before.facts.get(id));
          } else {
            const active = [...before.facts.values()].filter((f) => f.state === "active");
            const exact = typeof o.query === "object" ? active[o.query.exactOf % Math.max(1, active.length)] : undefined;
            const query = typeof o.query === "string" ? o.query : exact?.body ?? "router";
            const args: Record<string, unknown> = { query };
            if (o.tags) args.tags = o.tags;
            if (o.limit !== undefined) args.limit = o.limit;
            const reply = recallReply.parse(value(await call(client, "recall", args), "recall"));
            expect(reply.hits.length, `${where}: at most limit hits`).toBeLessThanOrEqual(o.limit ?? 10);
            for (const hit of reply.hits) {
              const m = /^fact:(\d+)$/.exec(hit.ref);
              expect(m, `${where}: only facts exist here, got ${hit.ref}`).not.toBeNull();
              const f = before.facts.get(Number(m![1]));
              expect(f, `${where}: ${hit.ref} resolves`).toBeDefined();
              expect(f?.state, `${where}: archived facts stay out of recall`).toBe("active");
              if (o.tags) expect(f?.tags.some((t) => o.tags!.includes(t)), `${where}: ${hit.ref} carries a requested tag`).toBe(true);
              const resolved = factReply.parse(value(await call(client, "get_fact", { id: Number(m![1]) }), "get_fact")).fact;
              expect(resolved.body, `${where}: the hit's text is the fact`).toBe(hit.text);
            }
            // Progress: an active fact asked for word for word is found.
            if (exact && !o.tags && (o.limit ?? 10) >= before.facts.size) {
              witnessed.add("recall finds a fact by its own words");
              expect(reply.hits.map((h) => h.ref), `${where}: recall finds fact:${exact.id} by its own body`).toContain(`fact:${exact.id}`);
            }
          }

          const problems = indexMirrorsFacts(await observe(pool));
          expect(problems, `${where}: index ≠ facts`).toEqual([]);
        }
      }),
      { numRuns: RUNS },
    );
    expect([...witnessed].sort()).toEqual(["a blank update is refused", "a fact is archived", "recall finds a fact by its own words"]);
  }, 600_000);

  // ── racing writes, slow embedder ─────────────────────────────────────────

  describe("racing writes", () => {
    let memory: MemoryModule;

    beforeAll(() => {
      // Bag-of-words vectors, as in the spec environment, after a random
      // delay — so a write's embedding can finish after a later write's.
      const slow: EmbeddingProvider = {
        dimensions: 1536,
        embed: async (text) => (await embedSlowly([text]))[0]!,
        embedBatch: (texts) => embedSlowly(texts),
      };
      memory = createMemoryModule({ db: server.pg.db, embeddingProvider: slow });
    });

    it("the index describes the last update of a fact, whatever order the embeddings finish in", async () => {
      await fc.assert(
        fc.asyncProperty(fc.array(fc.record({ body: sentence, state: fc.constantFrom("active" as const, "archived" as const) }), { minLength: 2, maxLength: 5 }), async (updates) => {
          await pool.query("TRUNCATE memory_facts, memory_index RESTART IDENTITY");
          const f = await memory.service.remember({ body: "the starting point" });
          await Promise.all(updates.map((u) => memory.service.updateFact({ id: f.id, body: u.body, state: u.state })));
          expect(indexMirrorsFacts(await observe(pool)), JSON.stringify(updates)).toEqual([]);
        }),
        { numRuns: 25 },
      );
    }, 300_000);

    // Re-indexing one unchanged source from several places at once (a write
    // and the backfill, two instances) must not interleave delete and insert.
    it("concurrent re-indexes of one source leave one set of rows", async () => {
      await pool.query("TRUNCATE memory_facts, memory_projects, memory_project_docs, memory_doc_patches, memory_index RESTART IDENTITY CASCADE");
      const f = await memory.service.remember({ body: "router password on the sticker" });
      await memory.service.createProject({ slug: "p", title: "P" });
      const written = await memory.service.writeDoc({ project: "p", doc: "notes.md", body: "## A\n\nalpha\n\n## B\n\nbeta", actor: "a" });
      const doc = await memory.service.readDoc("p", "notes.md");
      const project = (await memory.service.listProjects()).find((x) => x.slug === "p");
      if (!project) throw new Error("project p is missing");
      await Promise.all([
        ...Array.from({ length: 8 }, () => memory.indexer.indexFact(f)),
        ...Array.from({ length: 8 }, () => memory.indexer.indexDoc(project, doc)),
      ]);
      const { index } = await observe(pool);
      expect(index.filter((r) => r.sourceRef === `fact:${f.id}`)).toHaveLength(1);
      const chunks = index.filter((r) => r.sourceRef === "doc:p/notes.md").map((r) => r.ref);
      expect(chunks, `doc v${written.version}`).toEqual([...new Set(chunks)]);
    }, 120_000);

    it("the index describes the last write of a document, whatever order the embeddings finish in", async () => {
      await fc.assert(
        fc.asyncProperty(fc.array(sentence, { minLength: 2, maxLength: 5 }), async (lines) => {
          await pool.query("TRUNCATE memory_projects, memory_project_docs, memory_doc_patches, memory_index RESTART IDENTITY CASCADE");
          await memory.service.createProject({ slug: "p", title: "P" });
          await memory.service.writeDoc({ project: "p", doc: "notes.md", body: "start", actor: "a", rationale: "seed" });
          // Appends retry a lost compare-and-swap a few times; one that still
          // loses answers "raced" and changes nothing (the model allows it —
          // see F9). Whatever landed, the index is the final document's.
          const settled = await Promise.allSettled(
            lines.map((line, i) => memory.service.appendDoc({ project: "p", doc: "notes.md", text: `${line} #${i}`, actor: "a", rationale: "race" })),
          );
          const doc = await memory.service.readDoc("p", "notes.md");
          const { index } = await observe(pool);
          const indexed = index.filter((r) => r.sourceRef === "doc:p/notes.md").map((r) => r.text).join("\n");
          for (const [i, line] of lines.entries()) {
            const landed = settled[i]!.status === "fulfilled";
            expect(doc.body.includes(`${line} #${i}`), `append ${i} is in the document iff it succeeded`).toBe(landed);
            expect(indexed.includes(`${line} #${i}`), `index of the final document: ${JSON.stringify(lines)}`).toBe(landed);
          }
        }),
        { numRuns: 25 },
      );
    }, 300_000);
  });
});

async function embedSlowly(texts: string[]): Promise<number[][]> {
  await new Promise((resolve) => setTimeout(resolve, Math.random() * 30));
  return texts.map((text) => {
    const v = new Array<number>(1536).fill(0);
    for (const token of text.toLowerCase().split(/[^\p{L}\p{N}]+/u).filter(Boolean)) {
      let hash = 0;
      for (const c of token) hash = (hash * 31 + (c.codePointAt(0) ?? 0)) % 1536;
      v[hash] = (v[hash] ?? 0) + 1;
    }
    const norm = Math.max(Math.sqrt(v.reduce((s, x) => s + x * x, 0)), Number.EPSILON);
    return v.map((x) => x / norm);
  });
}
