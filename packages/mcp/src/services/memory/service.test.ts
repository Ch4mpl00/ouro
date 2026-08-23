import { beforeEach, describe, expect, it, vi } from "vitest";
import type { EmbeddingService } from "../embeddings/service";
import { createIndexer, type Indexer } from "./indexer";
import { MemoryError, createMemoryService, type MemoryService } from "./service";
import { createInMemoryStore, type InMemoryStoreOpts } from "./store.memory";
import type { MemoryStore } from "./store";

// Walks the acceptance list in .claude/tasks/unified-memory.md against a fake
// store. Everything asserted here is a rule of the service, not of Postgres.

// Deterministic stand-in for text-embedding-3-small: a bag-of-words vector, so
// "distance" tracks word overlap and a test can say which document a query
// should find without pinning real model output.
const DIMS = 64;
function fakeEmbed(text: string): number[] {
  const vector = new Array<number>(DIMS).fill(0);
  for (const token of text.toLowerCase().match(/[\p{L}\p{N}]+/gu) ?? []) {
    let hash = 0;
    for (const ch of token) hash = (hash * 31 + ch.codePointAt(0)!) % DIMS;
    vector[hash] = (vector[hash] ?? 0) + 1;
  }
  return vector;
}

interface Harness {
  service: MemoryService;
  store: MemoryStore;
  indexer: Indexer;
  embedBatch: ReturnType<typeof vi.fn>;
  // Flip to make every embed call throw, simulating an OpenAI outage.
  setProviderDown: (down: boolean) => void;
}

function harness(storeOpts: InMemoryStoreOpts = {}, store = createInMemoryStore(storeOpts)): Harness {
  let down = false;
  const embedBatch = vi.fn(async (texts: string[]) => {
    if (down) throw new Error("provider unreachable");
    return texts.map(fakeEmbed);
  });
  const embeddings: EmbeddingService = {
    embed: async (text) => fakeEmbed(text),
    embedBatch: embedBatch as unknown as EmbeddingService["embedBatch"],
  };
  const indexer = createIndexer({ store, embeddings });
  let patchSeq = 0;
  const service = createMemoryService({
    store,
    indexer,
    newPatchId: () => `pa:${(++patchSeq).toString(16).padStart(4, "0")}`,
  });
  return { service, store, indexer, embedBatch, setProviderDown: (value) => (down = value) };
}

const ACTOR = "supervisor";

async function expectMemoryError(promise: Promise<unknown>): Promise<MemoryError> {
  try {
    await promise;
  } catch (err) {
    if (err instanceof MemoryError) return err;
    throw err;
  }
  throw new Error("expected a MemoryError, but the call resolved");
}

describe("projects and documents", () => {
  let h: Harness;
  beforeEach(async () => {
    h = harness();
    await h.service.createProject({ slug: "leetcode-graphs", title: "Графы для интервью" });
  });

  it("round-trips a project: create, write docs, list, read", async () => {
    await h.service.writeDoc({
      project: "leetcode-graphs",
      doc: "passport.md",
      body: "# Паспорт\n\nЦель — пройти графы за месяц.\n",
      summary: "Цель и рамки проекта",
      actor: ACTOR,
    });
    await h.service.writeDoc({
      project: "leetcode-graphs",
      doc: "roadmap.md",
      body: "# Roadmap\n\n- [ ] BFS\n- [ ] Dijkstra\n",
      summary: "Список тем",
      actor: ACTOR,
    });

    const { project, docs } = await h.service.listDocs("leetcode-graphs");

    expect(project.title).toBe("Графы для интервью");
    // The registry is what stops notes.md / notes2.md multiplying, so it has
    // to carry enough to choose without opening anything.
    expect(docs.map((d) => ({ name: d.name, summary: d.summary, version: d.version }))).toEqual([
      { name: "passport.md", summary: "Цель и рамки проекта", version: 1 },
      { name: "roadmap.md", summary: "Список тем", version: 1 },
    ]);
    expect(docs[0]!.sizeBytes).toBeGreaterThan(0);

    const read = await h.service.readDoc("leetcode-graphs", "roadmap.md");
    expect(read.body).toBe("# Roadmap\n\n- [ ] BFS\n- [ ] Dijkstra\n");
    expect(read.version).toBe(1);
  });

  it("appends progress without a prior read and without a version", async () => {
    await h.service.writeDoc({
      project: "leetcode-graphs",
      doc: "progress.md",
      body: "# Прогресс\n\nДень 1: BFS\n",
      actor: ACTOR,
    });

    const written = await h.service.appendDoc({
      project: "leetcode-graphs",
      doc: "progress.md",
      text: "День 2: Dijkstra",
      actor: ACTOR,
    });

    expect(written.version).toBe(2);
    expect((await h.service.readDoc("leetcode-graphs", "progress.md")).body).toBe(
      "# Прогресс\n\nДень 1: BFS\n\nДень 2: Dijkstra\n",
    );
  });

  it("appends under a named heading", async () => {
    await h.service.writeDoc({
      project: "leetcode-graphs",
      doc: "notes.md",
      body: "## Прогресс\n\nДень 1\n\n## Ошибки\n\nЗабыл visited\n",
      actor: ACTOR,
    });

    await h.service.appendDoc({
      project: "leetcode-graphs",
      doc: "notes.md",
      text: "День 2",
      underHeading: "Прогресс",
      actor: ACTOR,
    });

    expect((await h.service.readDoc("leetcode-graphs", "notes.md")).body).toBe(
      "## Прогресс\n\nДень 1\n\nДень 2\n\n## Ошибки\n\nЗабыл visited\n",
    );
  });

  it("refuses to append to a document that does not exist, listing the ones that do", async () => {
    await h.service.writeDoc({ project: "leetcode-graphs", doc: "roadmap.md", body: "x\n", actor: ACTOR });

    const err = await expectMemoryError(
      h.service.appendDoc({ project: "leetcode-graphs", doc: "roadmap2.md", text: "y", actor: ACTOR }),
    );

    expect(err.code).toBe("doc_not_found");
    expect(err.details).toMatchObject({ docs: ["roadmap.md"] });
  });

  it("names the known projects when the slug is wrong", async () => {
    const err = await expectMemoryError(h.service.readDoc("leetcode-graph", "roadmap.md"));

    expect(err.code).toBe("project_not_found");
    expect(err.details).toMatchObject({ projects: ["leetcode-graphs"] });
  });

  it("rejects a slug or filename that could not round-trip through a ref", async () => {
    await expect(h.service.createProject({ slug: "Bad Slug", title: "x" })).rejects.toThrow(/Invalid project slug/);
    await expect(
      h.service.writeDoc({ project: "leetcode-graphs", doc: "../etc/passwd", body: "x", actor: ACTOR }),
    ).rejects.toThrow(/Invalid document name/);
  });

  it("refuses to create a project twice", async () => {
    const err = await expectMemoryError(
      h.service.createProject({ slug: "leetcode-graphs", title: "Другое" }),
    );
    expect(err.code).toBe("project_exists");
  });
});

describe("write_doc versioning", () => {
  let h: Harness;
  beforeEach(async () => {
    h = harness();
    await h.service.createProject({ slug: "p", title: "P" });
  });

  it("creates without a version and bumps to 1", async () => {
    const written = await h.service.writeDoc({ project: "p", doc: "a.md", body: "one\n", actor: ACTOR });
    expect(written.version).toBe(1);
  });

  // write_doc is the only op that can lose content; it never runs blind.
  it("requires expected_version once the document exists", async () => {
    await h.service.writeDoc({ project: "p", doc: "a.md", body: "one\n", actor: ACTOR });

    const err = await expectMemoryError(
      h.service.writeDoc({ project: "p", doc: "a.md", body: "two\n", actor: ACTOR }),
    );

    expect(err.code).toBe("version_required");
    expect(err.details).toMatchObject({ currentVersion: 1 });
    expect((await h.service.readDoc("p", "a.md")).body).toBe("one\n");
  });

  it("rejects a version aimed at a document that does not exist yet", async () => {
    const err = await expectMemoryError(
      h.service.writeDoc({ project: "p", doc: "new.md", body: "x", expectedVersion: 3, actor: ACTOR }),
    );
    expect(err.code).toBe("version_conflict");
  });
});

describe("patch_doc", () => {
  let h: Harness;
  const ROADMAP = "# Roadmap\n\n- [ ] BFS\n- [ ] Dijkstra\n- [ ] A*\n";
  beforeEach(async () => {
    h = harness();
    await h.service.createProject({ slug: "p", title: "P" });
    await h.service.writeDoc({ project: "p", doc: "roadmap.md", body: ROADMAP, actor: ACTOR });
  });

  it("applies edits and bumps the version", async () => {
    const written = await h.service.patchDoc({
      project: "p",
      doc: "roadmap.md",
      expectedVersion: 1,
      edits: [{ old: "- [ ] Dijkstra", new: "- [x] Dijkstra" }],
      rationale: "отметь, что Dijkstra пройден",
      actor: ACTOR,
    });

    expect(written.version).toBe(2);
    expect((await h.service.readDoc("p", "roadmap.md")).body).toContain("- [x] Dijkstra");
  });

  it("fails on a stale version and changes nothing", async () => {
    await h.service.appendDoc({ project: "p", doc: "roadmap.md", text: "- [ ] Floyd", actor: "other-agent" });

    const err = await expectMemoryError(
      h.service.patchDoc({
        project: "p",
        doc: "roadmap.md",
        expectedVersion: 1,
        edits: [{ old: "- [ ] Dijkstra", new: "- [x] Dijkstra" }],
        actor: ACTOR,
      }),
    );

    expect(err.code).toBe("version_conflict");
    expect(err.details).toMatchObject({ currentVersion: 2 });
    expect((await h.service.readDoc("p", "roadmap.md")).body).toContain("- [ ] Dijkstra");
  });

  it("fails on an ambiguous quote and reports how many times it matched", async () => {
    await h.service.writeDoc({
      project: "p",
      doc: "dup.md",
      body: "- [ ] review\n- [ ] review\n",
      actor: ACTOR,
    });

    const err = await expectMemoryError(
      h.service.patchDoc({
        project: "p",
        doc: "dup.md",
        expectedVersion: 1,
        edits: [{ old: "- [ ] review", new: "- [x] review" }],
        actor: ACTOR,
      }),
    );

    expect(err.code).toBe("edit_failed");
    expect(err.details).toMatchObject({ applied: false });
    expect(err.details.failures).toMatchObject([{ reason: "ambiguous", occurrences: 2 }]);
  });

  it("returns the literal that would have matched when the quote was normalised", async () => {
    await h.service.writeDoc({ project: "p", doc: "goal.md", body: "Цель — пройти графы.\n", actor: ACTOR });

    const err = await expectMemoryError(
      h.service.patchDoc({
        project: "p",
        doc: "goal.md",
        expectedVersion: 1,
        edits: [{ old: "Цель - пройти графы.", new: "Цель — пройти графы за месяц." }],
        actor: ACTOR,
      }),
    );

    expect(err.details.failures).toMatchObject([
      { reason: "not_found", suggestions: ["Цель — пройти графы."] },
    ]);
  });

  it("leaves the document untouched when one edit of several fails", async () => {
    const err = await expectMemoryError(
      h.service.patchDoc({
        project: "p",
        doc: "roadmap.md",
        expectedVersion: 1,
        edits: [
          { old: "- [ ] BFS", new: "- [x] BFS" },
          { old: "- [ ] Floyd", new: "- [x] Floyd" },
        ],
        actor: ACTOR,
      }),
    );

    expect(err.code).toBe("edit_failed");
    const doc = await h.service.readDoc("p", "roadmap.md");
    expect(doc.body).toBe(ROADMAP);
    expect(doc.version).toBe(1);
  });
});

// Two agents share one memory (D6), so the version check is not paperwork.
describe("concurrent writers", () => {
  it("append_doc absorbs a write that lands between its read and its commit", async () => {
    let sneaked = false;
    const store = createInMemoryStore({
      onBeforeUpdateDoc: async (docId) => {
        if (sneaked) return;
        sneaked = true;
        // Another agent commits in the window. The CAS must fail and
        // append_doc must re-read rather than clobber it.
        const current = await store.getDoc(1, "log.md");
        if (current?.id === docId) {
          await store.updateDoc({
            docId,
            expectedVersion: current.version,
            body: `${current.body}\nfrom other agent\n`,
          });
        }
      },
    });
    const h = harness({}, store);
    await h.service.createProject({ slug: "p", title: "P" });
    await h.service.writeDoc({ project: "p", doc: "log.md", body: "start\n", actor: ACTOR });

    await h.service.appendDoc({ project: "p", doc: "log.md", text: "mine", actor: ACTOR });

    const body = (await h.service.readDoc("p", "log.md")).body;
    expect(body).toContain("from other agent");
    expect(body).toContain("mine");
  });
});

describe("history and revert", () => {
  let h: Harness;
  beforeEach(async () => {
    h = harness();
    await h.service.createProject({ slug: "p", title: "P" });
    await h.service.writeDoc({
      project: "p",
      doc: "roadmap.md",
      body: "- [ ] BFS\n- [ ] Dijkstra\n",
      actor: ACTOR,
    });
  });

  it("lists patches newest-first with ids, actors and rationales", async () => {
    await h.service.patchDoc({
      project: "p",
      doc: "roadmap.md",
      expectedVersion: 1,
      edits: [{ old: "- [ ] BFS", new: "- [x] BFS" }],
      rationale: "BFS пройден",
      actor: "claude-code@laptop",
    });

    const history = await h.service.history("p", "roadmap.md");

    expect(history).toMatchObject([
      {
        kind: "patch",
        actor: "claude-code@laptop",
        rationale: "BFS пройден",
        versionBefore: 1,
        versionAfter: 2,
        editCount: 1,
      },
      { kind: "write", versionBefore: 0, versionAfter: 1 },
    ]);
    expect(history[0]!.patchId).toMatch(/^pa:/);
  });

  it("reverts the newest patch exactly", async () => {
    const patched = await h.service.patchDoc({
      project: "p",
      doc: "roadmap.md",
      expectedVersion: 1,
      edits: [{ old: "- [ ] BFS", new: "- [x] BFS" }],
      actor: ACTOR,
    });

    await h.service.revert({ project: "p", doc: "roadmap.md", patchId: patched.patchId, actor: ACTOR });

    const doc = await h.service.readDoc("p", "roadmap.md");
    expect(doc.body).toBe("- [ ] BFS\n- [ ] Dijkstra\n");
    // Reverting is itself a patch — history is never rewritten.
    expect(doc.version).toBe(3);
    expect((await h.service.history("p", "roadmap.md"))[0]).toMatchObject({ kind: "revert" });
  });

  it("reverts a mid-stack patch whose text later patches did not touch", async () => {
    const first = await h.service.patchDoc({
      project: "p",
      doc: "roadmap.md",
      expectedVersion: 1,
      edits: [{ old: "- [ ] BFS", new: "- [x] BFS" }],
      actor: ACTOR,
    });
    await h.service.patchDoc({
      project: "p",
      doc: "roadmap.md",
      expectedVersion: 2,
      edits: [{ old: "- [ ] Dijkstra", new: "- [x] Dijkstra" }],
      actor: ACTOR,
    });

    await h.service.revert({ project: "p", doc: "roadmap.md", patchId: first.patchId, actor: ACTOR });

    // Only the reverted change is undone; the later one survives.
    expect((await h.service.readDoc("p", "roadmap.md")).body).toBe("- [ ] BFS\n- [x] Dijkstra\n");
  });

  // A later patch that merely *extended* the line still contains the text the
  // inverse looks for, so the revert applies and keeps the later addition —
  // the same outcome `git revert` gives on a hunk that still applies.
  it("reverts through a later edit that only extended the same line", async () => {
    const first = await h.service.patchDoc({
      project: "p",
      doc: "roadmap.md",
      expectedVersion: 1,
      edits: [{ old: "- [ ] BFS", new: "- [x] BFS" }],
      actor: ACTOR,
    });
    await h.service.patchDoc({
      project: "p",
      doc: "roadmap.md",
      expectedVersion: 2,
      edits: [{ old: "- [x] BFS", new: "- [x] BFS (повторить)" }],
      actor: ACTOR,
    });

    await h.service.revert({ project: "p", doc: "roadmap.md", patchId: first.patchId, actor: ACTOR });

    expect((await h.service.readDoc("p", "roadmap.md")).body).toBe("- [ ] BFS (повторить)\n- [ ] Dijkstra\n");
  });

  it("fails cleanly on a conflicting mid-stack revert and offers a rollback version", async () => {
    const first = await h.service.patchDoc({
      project: "p",
      doc: "roadmap.md",
      expectedVersion: 1,
      edits: [{ old: "- [ ] BFS", new: "- [x] BFS" }],
      actor: ACTOR,
    });
    // This one removes the anchor the inverse would need entirely.
    await h.service.patchDoc({
      project: "p",
      doc: "roadmap.md",
      expectedVersion: 2,
      edits: [{ old: "- [x] BFS", new: "- [x] Обход в ширину" }],
      actor: ACTOR,
    });

    const err = await expectMemoryError(
      h.service.revert({ project: "p", doc: "roadmap.md", patchId: first.patchId, actor: ACTOR }),
    );

    expect(err.code).toBe("revert_conflict");
    expect(err.details).toMatchObject({ rollbackToVersion: 1 });
    // Nothing was written — a partial revert is never applied.
    expect((await h.service.readDoc("p", "roadmap.md")).body).toBe("- [x] Обход в ширину\n- [ ] Dijkstra\n");
  });

  it("rolls the whole document back when the caller opts in", async () => {
    const first = await h.service.patchDoc({
      project: "p",
      doc: "roadmap.md",
      expectedVersion: 1,
      edits: [{ old: "- [ ] BFS", new: "- [x] BFS" }],
      actor: ACTOR,
    });
    await h.service.patchDoc({
      project: "p",
      doc: "roadmap.md",
      expectedVersion: 2,
      edits: [{ old: "- [x] BFS", new: "- [x] Обход в ширину" }],
      actor: ACTOR,
    });

    await h.service.revert({
      project: "p",
      doc: "roadmap.md",
      patchId: first.patchId,
      rollback: true,
      actor: ACTOR,
    });

    expect((await h.service.readDoc("p", "roadmap.md")).body).toBe("- [ ] BFS\n- [ ] Dijkstra\n");
  });

  it("cannot undo an append in place, but names the rollback that would work", async () => {
    const appended = await h.service.appendDoc({
      project: "p",
      doc: "roadmap.md",
      text: "- [ ] Floyd",
      actor: ACTOR,
    });
    await h.service.appendDoc({ project: "p", doc: "roadmap.md", text: "- [ ] Kruskal", actor: ACTOR });

    const err = await expectMemoryError(
      h.service.revert({ project: "p", doc: "roadmap.md", patchId: appended.patchId, actor: ACTOR }),
    );

    expect(err.code).toBe("revert_conflict");
    expect(err.details).toMatchObject({ rollbackToVersion: 1 });
  });

  // The model will hallucinate ids; an unknown one is a hard error.
  it("rejects an unknown patch id and lists the real ones", async () => {
    const err = await expectMemoryError(
      h.service.revert({ project: "p", doc: "roadmap.md", patchId: "pa:beef", actor: ACTOR }),
    );

    expect(err.code).toBe("patch_not_found");
    expect(err.details.knownPatchIds).toHaveLength(1);
  });
});

describe("facts", () => {
  let h: Harness;
  beforeEach(() => {
    h = harness();
  });

  it("remembers, reads back and updates a fact", async () => {
    const fact = await h.service.remember({ body: "Пароль от роутера в сейфе", tags: ["дом", "дом"] });

    expect(fact.state).toBe("active");
    // Tags are de-duplicated but never re-worded.
    expect(fact.tags).toEqual(["дом"]);
    expect((await h.service.getFact(fact.id)).body).toBe("Пароль от роутера в сейфе");

    const updated = await h.service.updateFact({ id: fact.id, state: "archived" });
    expect(updated.state).toBe("archived");
  });

  it("refuses an empty fact", async () => {
    expect((await expectMemoryError(h.service.remember({ body: "   " }))).code).toBe("empty_body");
  });

  it("reports a missing fact rather than returning null", async () => {
    expect((await expectMemoryError(h.service.getFact(999))).code).toBe("fact_not_found");
  });
});

describe("recall", () => {
  let h: Harness;
  beforeEach(async () => {
    h = harness();
    await h.service.createProject({ slug: "graphs", title: "Графы для интервью" });
    await h.service.writeDoc({
      project: "graphs",
      doc: "progress.md",
      body: "# Прогресс\n\nЗастрял на Dijkstra, не понимаю релаксацию рёбер\n",
      actor: ACTOR,
    });
    await h.service.remember({ body: "Купить молоко и хлеб", tags: ["покупки"] });
    await h.service.remember({ body: "Dijkstra работает только на неотрицательных весах", tags: ["графы"] });
  });

  it("spans documents and facts, and its refs resolve to full content", async () => {
    const hits = await h.service.recall({ query: "Dijkstra релаксация рёбер" });

    const refs = hits.map((hit) => hit.ref);
    expect(refs.some((ref) => ref.startsWith("doc:graphs/progress.md#"))).toBe(true);
    expect(refs.some((ref) => ref.startsWith("fact:"))).toBe(true);

    // Following a ref is the second half of the search→read pattern.
    const doc = await h.service.readDoc("graphs", "progress.md");
    expect(doc.body).toContain("релаксацию рёбер");
    const factId = Number(refs.find((r) => r.startsWith("fact:"))!.slice("fact:".length));
    expect((await h.service.getFact(factId)).body).toContain("Dijkstra");
  });

  it("puts the on-topic hit first and the shopping list nowhere near it", async () => {
    const [top] = await h.service.recall({ query: "Dijkstra неотрицательные веса" });

    expect(top?.text).toContain("Dijkstra");
  });

  it("hides archived facts by default and finds them on request", async () => {
    const fact = await h.service.remember({ body: "Старый вайфай пароль qwerty123", tags: ["дом"] });
    await h.service.updateFact({ id: fact.id, state: "archived" });

    const active = await h.service.recall({ query: "вайфай пароль qwerty123" });
    expect(active.map((h_) => h_.ref)).not.toContain(`fact:${fact.id}`);

    const archived = await h.service.recall({ query: "вайфай пароль qwerty123", states: ["archived"] });
    expect(archived.map((h_) => h_.ref)).toContain(`fact:${fact.id}`);
  });

  it("filters by tag when asked", async () => {
    const hits = await h.service.recall({ query: "Dijkstra", tags: ["покупки"] });

    expect(hits.every((hit) => hit.ref.startsWith("fact:"))).toBe(true);
    expect(hits.map((hit) => hit.text)).toEqual(["Купить молоко и хлеб"]);
  });

  it("honours the limit", async () => {
    expect(await h.service.recall({ query: "Dijkstra", limit: 1 })).toHaveLength(1);
  });

  it("drops the old chunks of a document it re-indexes", async () => {
    await h.service.writeDoc({
      project: "graphs",
      doc: "progress.md",
      body: "# Прогресс\n\nВсё переписано, теперь про топологическую сортировку\n",
      expectedVersion: 1,
      actor: ACTOR,
    });

    const hits = await h.service.recall({ query: "Dijkstra релаксация рёбер" });

    expect(hits.map((hit) => hit.text).join("\n")).not.toContain("релаксацию рёбер");
  });
});

// D4: reads and patches must never depend on OpenAI being up. Only
// searchability is allowed to lag.
describe("with the embedding provider down", () => {
  let h: Harness;
  beforeEach(async () => {
    h = harness();
    await h.service.createProject({ slug: "p", title: "P" });
  });

  it("still writes, reads and patches documents", async () => {
    h.setProviderDown(true);

    await h.service.writeDoc({ project: "p", doc: "a.md", body: "- [ ] one\n", actor: ACTOR });
    await h.service.patchDoc({
      project: "p",
      doc: "a.md",
      expectedVersion: 1,
      edits: [{ old: "- [ ] one", new: "- [x] one" }],
      actor: ACTOR,
    });

    expect((await h.service.readDoc("p", "a.md")).body).toBe("- [x] one\n");
  });

  it("says search is unavailable instead of pretending memory is empty", async () => {
    await h.service.remember({ body: "что-то важное" });
    h.setProviderDown(true);

    expect((await expectMemoryError(h.service.recall({ query: "важное" }))).code).toBe("search_unavailable");
  });

  it("catches up on backfill once the provider returns", async () => {
    h.setProviderDown(true);
    await h.service.writeDoc({ project: "p", doc: "a.md", body: "# Заметка\n\nпро Dijkstra\n", actor: ACTOR });
    await h.service.remember({ body: "Dijkstra не любит отрицательные веса" });
    expect(await h.service.recall({ query: "Dijkstra" }).catch((e: MemoryError) => e.code)).toBe(
      "search_unavailable",
    );

    h.setProviderDown(false);
    const drained = await h.indexer.embedMissingBatch();

    expect(drained).toEqual({ embedded: 2, failed: 0 });
    const hits = await h.service.recall({ query: "Dijkstra" });
    expect(hits.length).toBe(2);
  });

  it("reports nothing left to do when the backlog is empty", async () => {
    await h.service.remember({ body: "уже встроено" });

    expect(await h.indexer.embedMissingBatch()).toEqual({ embedded: 0, failed: 0 });
  });
});

// The point of the whole task: a different agent, in a new session, picks up
// where the last one stopped instead of restarting from zero.
describe("hand-off between agents", () => {
  it("lets a second session read the project and continue it", async () => {
    const store = createInMemoryStore();
    const first = harness({}, store);
    await first.service.createProject({ slug: "graphs", title: "Графы" });
    await first.service.writeDoc({
      project: "graphs",
      doc: "roadmap.md",
      body: "# Roadmap\n\n- [x] BFS\n- [ ] Dijkstra\n",
      summary: "Темы и статус",
      actor: "supervisor",
    });
    await first.service.writeDoc({
      project: "graphs",
      doc: "progress.md",
      body: "# Прогресс\n\nДень 1: BFS разобран\n",
      summary: "Дневник",
      actor: "supervisor",
    });

    // A cold session: nothing shared but the store.
    const second = harness({}, store);
    const { docs } = await second.service.listDocs("graphs");
    expect(docs.map((d) => d.name)).toEqual(["progress.md", "roadmap.md"]);

    const roadmap = await second.service.readDoc("graphs", "roadmap.md");
    await second.service.patchDoc({
      project: "graphs",
      doc: "roadmap.md",
      expectedVersion: roadmap.version,
      edits: [{ old: "- [ ] Dijkstra", new: "- [x] Dijkstra" }],
      rationale: "продолжаю с того места, где остановились",
      actor: "claude-code@laptop",
    });
    await second.service.appendDoc({
      project: "graphs",
      doc: "progress.md",
      text: "День 2: Dijkstra разобран",
      actor: "claude-code@laptop",
    });

    // Back on the first session's view of the same memory.
    expect((await first.service.readDoc("graphs", "roadmap.md")).body).toContain("- [x] Dijkstra");
    expect((await first.service.readDoc("graphs", "progress.md")).body).toContain("День 2");
    expect((await first.service.history("graphs", "roadmap.md"))[0]).toMatchObject({
      actor: "claude-code@laptop",
      rationale: "продолжаю с того места, где остановились",
    });
  });
});
