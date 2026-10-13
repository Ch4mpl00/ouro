// The journal's invariants, checked by fast-check against a reference model on
// random call sequences. The model is the whole specification of a stream —
// a list that keeps the newest `capacity` entries — so any disagreement is a
// bug in the service or the store, and fast-check prints the shortest call
// sequence that shows it.
//
//   I1 entries are immutable: a surviving entry reads back exactly as appended
//   I2 tail is newest-first, by append order
//   I3 a stream never exceeds its capacity, and only the oldest entries leave
//   I5 concurrent appends all land; the newest `capacity` survive
//   I6 streams are independent
//   I9 an invalid stream name is rejected and writes nothing

import fc from "fast-check";
import { describe, expect, it } from "vitest";
import type { EmbeddingService } from "../embeddings/service";
import { createIndexer } from "./indexer";
import { JOURNAL_STREAM_RE } from "./refs";
import { JOURNAL_DEFAULT_CAPACITY, MemoryError, createMemoryService, type MemoryService } from "./service";
import { createInMemoryStore } from "./store.memory";
import type { JournalEntry } from "./types";

const RUNS = Number(process.env.MODEL_CASES ?? 300);
const ACTOR = "supervisor";
// Few streams, one a prefix of another, so sequences actually collide (I6).
const STREAMS = ["digest", "digest/news", "log"] as const;

const unusedEmbeddings: EmbeddingService = {
  embed: async () => {
    throw new Error("the journal must not embed");
  },
  embedBatch: async () => {
    throw new Error("the journal must not embed");
  },
};

function freshService(): MemoryService {
  const store = createInMemoryStore();
  return createMemoryService({ store, indexer: createIndexer({ store, embeddings: unusedEmbeddings }) });
}

type Op =
  | { op: "append"; stream: number; body: string; capacity?: number }
  | { op: "tail"; stream: number; n: number };

// Small capacities so trimming happens within a 30-step sequence.
const opArb: fc.Arbitrary<Op> = fc.oneof(
  {
    weight: 3,
    arbitrary: fc.record(
      {
        op: fc.constant("append" as const),
        stream: fc.nat({ max: STREAMS.length - 1 }),
        body: fc.string({ minLength: 1, maxLength: 12 }).filter((s) => s.trim().length > 0),
        capacity: fc.integer({ min: 1, max: 6 }),
      },
      { requiredKeys: ["op", "stream", "body"] },
    ),
  },
  {
    weight: 1,
    arbitrary: fc.record({
      op: fc.constant("tail" as const),
      stream: fc.nat({ max: STREAMS.length - 1 }),
      n: fc.integer({ min: 1, max: 10 }),
    }),
  },
);

// The reference model: per stream, a capacity and the surviving entries in
// append order. Entries are the service's own return values, so comparing
// them checks immutability (I1) for free.
interface ModelStream {
  capacity: number;
  entries: JournalEntry[];
}

describe("journal — properties against the reference model", () => {
  it("behaves like a bounded list per stream (I1, I2, I3, I6)", async () => {
    await fc.assert(
      fc.asyncProperty(fc.array(opArb, { maxLength: 30 }), async (ops) => {
        const service = freshService();
        const model = new Map<string, ModelStream>();

        for (const op of ops) {
          const stream = STREAMS[op.stream]!;
          if (op.op === "append") {
            const out = await service.journalAppend({ stream, body: op.body, actor: ACTOR, capacity: op.capacity });
            const m = model.get(stream) ?? { capacity: JOURNAL_DEFAULT_CAPACITY, entries: [] };
            if (op.capacity !== undefined) m.capacity = op.capacity;
            m.entries.push(out.entry);
            m.entries = m.entries.slice(-m.capacity);
            model.set(stream, m);
            expect(out.entry.body).toBe(op.body);
            expect(out.stream).toMatchObject({ name: stream, capacity: m.capacity, size: m.entries.length });
          } else {
            const { entries } = await service.journalTail({ stream, n: op.n });
            const expected = (model.get(stream)?.entries ?? []).slice(-op.n).reverse();
            expect(entries).toEqual(expected);
          }
        }

        // The whole state, not just what the sequence happened to tail.
        for (const name of STREAMS) {
          const m = model.get(name);
          const { entries } = await service.journalTail({ stream: name, n: 100 });
          expect(entries).toEqual([...(m?.entries ?? [])].reverse());
          expect(entries.length).toBeLessThanOrEqual(m?.capacity ?? 0);
        }
        const listed = await service.listJournals();
        expect(listed.map((s) => [s.name, s.capacity, s.size]).sort()).toEqual(
          [...model].map(([name, m]) => [name, m.capacity, m.entries.length]).sort(),
        );
      }),
      { numRuns: RUNS },
    );
  });

  it("keeps the newest `capacity` entries when appends race (I5)", async () => {
    await fc.assert(
      fc.asyncProperty(fc.integer({ min: 1, max: 8 }), fc.integer({ min: 1, max: 25 }), async (capacity, count) => {
        const service = freshService();
        await service.journalAppend({ stream: "race", body: "seed", actor: ACTOR, capacity });
        const results = await Promise.all(
          Array.from({ length: count }, (_, i) => service.journalAppend({ stream: "race", body: `p${i}`, actor: ACTOR })),
        );
        const { entries } = await service.journalTail({ stream: "race", n: 100 });
        const appended = results.map((r) => r.entry.id);
        expect(new Set(appended).size).toBe(count);
        expect(entries).toHaveLength(Math.min(capacity, count + 1));
        if (count >= capacity) {
          const newest = appended.sort((a, b) => b - a).slice(0, capacity);
          expect(entries.map((e) => e.id)).toEqual(newest);
        }
      }),
      { numRuns: RUNS },
    );
  });

  it("accepts exactly the names the grammar allows, and a rejected call writes nothing (I9)", async () => {
    const name = fc.oneof(
      fc.stringMatching(JOURNAL_STREAM_RE).filter((s) => s.length <= 100),
      fc.string({ maxLength: 20 }),
      fc.string({ unit: fc.constantFrom("a", "/", "-", "1", "A", "."), maxLength: 8 }),
    );
    await fc.assert(
      fc.asyncProperty(name, async (stream) => {
        const service = freshService();
        const valid = JOURNAL_STREAM_RE.test(stream) && stream.length <= 100;
        try {
          await service.journalAppend({ stream, body: "x", actor: ACTOR });
          expect(valid).toBe(true);
          expect((await service.listJournals()).map((s) => s.name)).toEqual([stream]);
        } catch (err) {
          if (!(err instanceof MemoryError)) throw err;
          expect(err.code).toBe("invalid_stream");
          expect(valid).toBe(false);
          expect(await service.listJournals()).toEqual([]);
        }
      }),
      { numRuns: RUNS },
    );
  });
});
