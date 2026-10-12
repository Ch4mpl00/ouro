import { describe, expect, it, vi } from "vitest";
import type { EmbeddingService } from "../embeddings/service";
import { createIndexer } from "./indexer";
import {
  JOURNAL_DEFAULT_CAPACITY,
  JOURNAL_MAX_BODY_CHARS,
  JOURNAL_MAX_CAPACITY,
  JOURNAL_TAIL_DEFAULT,
  JOURNAL_TAIL_MAX,
  MemoryError,
  createMemoryService,
  type MemoryService,
} from "./service";
import { createInMemoryStore } from "./store.memory";

// The journal's requirements, one test each (F = functional, N = non-
// functional, I = invariant). The randomised versions of the invariants live
// in journal.properties.test.ts; these pin the examples a reader checks first.

const ACTOR = "supervisor";

function harness() {
  const store = createInMemoryStore();
  const embeddings = {
    embed: vi.fn<EmbeddingService["embed"]>(async () => [1, 0, 0]),
    embedBatch: vi.fn<EmbeddingService["embedBatch"]>(async (texts) => texts.map(() => [1, 0, 0])),
  };
  const service = createMemoryService({ store, indexer: createIndexer({ store, embeddings }) });
  return { service, embeddings };
}

async function memoryError(promise: Promise<unknown>): Promise<MemoryError> {
  try {
    await promise;
  } catch (err) {
    if (err instanceof MemoryError) return err;
    throw err;
  }
  throw new Error("expected a MemoryError, the call succeeded");
}

const append = (service: MemoryService, stream: string, body: string, capacity?: number) =>
  service.journalAppend({ stream, body, actor: ACTOR, capacity });

const bodies = async (service: MemoryService, stream: string, n?: number) =>
  (await service.journalTail({ stream, n })).entries.map((e) => e.body);

describe("journal — functional", () => {
  it("F1 append returns the entry and the stream it landed in", async () => {
    const { service } = harness();
    const out = await append(service, "news-digest", "digest #1");
    expect(out.entry).toMatchObject({ stream: "news-digest", body: "digest #1", actor: ACTOR });
    expect(out.entry.id).toBeGreaterThan(0);
    expect(out.stream).toMatchObject({ name: "news-digest", capacity: JOURNAL_DEFAULT_CAPACITY, size: 1 });
  });

  it("F2 tail returns the last n entries, newest first", async () => {
    const { service } = harness();
    for (const b of ["a", "b", "c", "d"]) await append(service, "s", b);
    expect(await bodies(service, "s", 2)).toEqual(["d", "c"]);
    expect(await bodies(service, "s", 10)).toEqual(["d", "c", "b", "a"]);
  });

  it(`F2 tail defaults to ${JOURNAL_TAIL_DEFAULT} entries and caps n at ${JOURNAL_TAIL_MAX}`, async () => {
    const { service } = harness();
    for (let i = 0; i < JOURNAL_TAIL_DEFAULT + 3; i++) await append(service, "s", `e${i}`);
    expect(await bodies(service, "s")).toHaveLength(JOURNAL_TAIL_DEFAULT);
    expect(await memoryError(service.journalTail({ stream: "s", n: JOURNAL_TAIL_MAX + 1 }))).toMatchObject({
      code: "invalid_n",
    });
    expect(await memoryError(service.journalTail({ stream: "s", n: 0 }))).toMatchObject({ code: "invalid_n" });
  });

  it("F3 a stream needs no setup: the first append creates it, a missing one reads empty", async () => {
    const { service } = harness();
    expect(await service.journalTail({ stream: "never-written" })).toEqual({ stream: "never-written", entries: [] });
    await append(service, "fresh", "x");
    expect(await bodies(service, "fresh")).toEqual(["x"]);
  });

  it("F4 capacity is a property of the stream: set once, kept by later appends", async () => {
    const { service } = harness();
    await append(service, "s", "a", 2);
    const out = await append(service, "s", "b");
    expect(out.stream.capacity).toBe(2);
    await append(service, "s", "c");
    expect(await bodies(service, "s")).toEqual(["c", "b"]);
  });

  it("F4 lowering the capacity trims the stream on that append", async () => {
    const { service } = harness();
    for (const b of ["a", "b", "c", "d"]) await append(service, "s", b);
    const out = await append(service, "s", "e", 2);
    expect(out.stream).toMatchObject({ capacity: 2, size: 2 });
    expect(await bodies(service, "s")).toEqual(["e", "d"]);
  });

  it(`F4 capacity must be an integer in 1..${JOURNAL_MAX_CAPACITY}`, async () => {
    const { service } = harness();
    for (const capacity of [0, -1, 1.5, JOURNAL_MAX_CAPACITY + 1]) {
      expect(await memoryError(append(service, "s", "x", capacity))).toMatchObject({ code: "invalid_capacity" });
    }
    expect(await service.listJournals()).toEqual([]);
  });

  it("F5 listJournals names every stream with its capacity, size and last write", async () => {
    const { service } = harness();
    await append(service, "news-digest", "a");
    await append(service, "digest/tech", "b", 3);
    await append(service, "digest/tech", "c");
    const listed = await service.listJournals();
    expect(listed.map(({ lastAt, ...rest }) => ({ ...rest, hasLastAt: lastAt !== null }))).toEqual([
      { name: "digest/tech", capacity: 3, size: 2, hasLastAt: true },
      { name: "news-digest", capacity: JOURNAL_DEFAULT_CAPACITY, size: 1, hasLastAt: true },
    ]);
  });

  it("F6 the actor is recorded on every entry", async () => {
    const { service } = harness();
    await service.journalAppend({ stream: "s", body: "x", actor: "chatgpt" });
    expect((await service.journalTail({ stream: "s" })).entries[0]?.actor).toBe("chatgpt");
  });
});

describe("journal — non-functional", () => {
  it("N1 writing and reading the journal never calls the embedder", async () => {
    const { service, embeddings } = harness();
    await append(service, "s", "a digest about Odesa power outages");
    await service.journalTail({ stream: "s" });
    await service.listJournals();
    expect(embeddings.embed).not.toHaveBeenCalled();
    expect(embeddings.embedBatch).not.toHaveBeenCalled();
  });

  it(`N3 a body is non-empty and at most ${JOURNAL_MAX_BODY_CHARS} characters`, async () => {
    const { service } = harness();
    expect(await memoryError(append(service, "s", "  \n"))).toMatchObject({ code: "empty_body" });
    expect(await memoryError(append(service, "s", "x".repeat(JOURNAL_MAX_BODY_CHARS + 1)))).toMatchObject({
      code: "body_too_large",
    });
    await append(service, "s", "x".repeat(JOURNAL_MAX_BODY_CHARS));
    expect((await service.journalTail({ stream: "s" })).entries).toHaveLength(1);
  });

  it("N3 the body is stored exactly as given — a journal does not reformat records", async () => {
    const { service } = harness();
    await append(service, "s", "  line one\n\nline two  \n");
    expect(await bodies(service, "s")).toEqual(["  line one\n\nline two  \n"]);
  });
});

describe("journal — invariants (examples)", () => {
  it("I3 a stream never holds more than its capacity; only the oldest leave", async () => {
    const { service } = harness();
    for (let i = 0; i < 7; i++) await append(service, "s", `e${i}`, 3);
    expect(await bodies(service, "s", 10)).toEqual(["e6", "e5", "e4"]);
  });

  it("I5 concurrent appends all land, and the newest survive the trim", async () => {
    const { service } = harness();
    await append(service, "s", "seed", 5);
    const results = await Promise.all(Array.from({ length: 20 }, (_, i) => append(service, "s", `p${i}`)));
    const tail = (await service.journalTail({ stream: "s", n: 10 })).entries;
    expect(tail).toHaveLength(5);
    const newest = results
      .map((r) => r.entry.id)
      .sort((a, b) => b - a)
      .slice(0, 5);
    expect(tail.map((e) => e.id)).toEqual(newest);
  });

  it("I6 writing one stream never trims another, even one sharing a prefix", async () => {
    const { service } = harness();
    await append(service, "digest", "keep me", 1);
    for (let i = 0; i < 5; i++) await append(service, "digest/news", `n${i}`, 1);
    expect(await bodies(service, "digest")).toEqual(["keep me"]);
  });

  it("I7 recall never returns journal entries", async () => {
    const { service } = harness();
    await append(service, "s", "Odesa power outages schedule");
    expect(await service.recall({ query: "Odesa power outages schedule" })).toEqual([]);
  });

  it("I8 the journal is not a project: it never shows up in list_memory's projects", async () => {
    const { service } = harness();
    await append(service, "s", "x");
    expect(await service.listProjects()).toEqual([]);
  });

  it("I9 stream names: lowercase segments joined by '/', at most 100 characters", async () => {
    const { service } = harness();
    for (const ok of ["news-digest", "digest/news", "a/b/c-1", "x".repeat(100)]) {
      await append(service, ok, "x");
    }
    for (const bad of ["", "News", "a//b", "/a", "a/", "-a", "a/-b", "a b", "a.b", "x".repeat(101)]) {
      expect(await memoryError(append(service, bad, "x"))).toMatchObject({ code: "invalid_stream" });
      expect(await memoryError(service.journalTail({ stream: bad }))).toMatchObject({ code: "invalid_stream" });
    }
    expect((await service.listJournals()).map((s) => s.name).sort()).toEqual(
      ["news-digest", "digest/news", "a/b/c-1", "x".repeat(100)].sort(),
    );
  });
});
