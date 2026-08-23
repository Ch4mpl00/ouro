import { describe, expect, it } from "vitest";
import type { EmbeddingService } from "../embeddings/service";
import { importLegacyNotes, legacyNoteSource, type LegacyNote } from "./import-notes";
import { createIndexer } from "./indexer";
import { createMemoryService } from "./service";
import { createInMemoryStore } from "./store.memory";

function harness() {
  const store = createInMemoryStore();
  const embeddings: EmbeddingService = {
    embed: async () => [1, 0],
    embedBatch: async (texts) => texts.map(() => [1, 0]),
  };
  const indexer = createIndexer({ store, embeddings });
  return { store, indexer, service: createMemoryService({ store, indexer }) };
}

const NOTES: LegacyNote[] = [
  { id: 1, body: "Пароль от роутера в сейфе", tags: ["дом"], source: "telegram" },
  { id: 2, body: "Dijkstra не любит отрицательные веса", tags: ["графы"], source: null },
];

describe("importLegacyNotes", () => {
  it("copies notes verbatim, keeping tags and recording provenance", async () => {
    const h = harness();

    expect(await importLegacyNotes(NOTES, h)).toEqual({ imported: 2, skipped: 0 });

    const fact = await h.store.getFactBySource(legacyNoteSource(1));
    // Verbatim matters: the original phrasing is what a later structuring
    // pass has to work from.
    expect(fact).toMatchObject({
      body: "Пароль от роутера в сейфе",
      tags: ["дом"],
      source: "knowledge_base_notes:1",
      state: "active",
    });
  });

  it("makes the imported notes findable through recall", async () => {
    const h = harness();
    await importLegacyNotes(NOTES, h);

    const hits = await h.service.recall({ query: "что там про роутер" });

    expect(hits.map((hit) => hit.text)).toContain("Пароль от роутера в сейфе");
  });

  // A half-finished import must be safe to re-run — that is the whole reason
  // provenance is stored rather than just copied.
  it("is idempotent", async () => {
    const h = harness();
    await importLegacyNotes(NOTES, h);

    expect(await importLegacyNotes(NOTES, h)).toEqual({ imported: 0, skipped: 2 });
    expect(await h.service.recall({ query: "роутер" })).toHaveLength(2);
  });

  it("skips an empty note instead of creating an empty fact", async () => {
    const h = harness();

    expect(await importLegacyNotes([{ id: 9, body: "   ", tags: [], source: null }], h)).toEqual({
      imported: 0,
      skipped: 1,
    });
  });
});
