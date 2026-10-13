// Keeps `memory_index` in step with the read models (D4/D8).
//
// The one background-ish concern in v1: chunk, denormalise, embed. It runs
// inline after each write and degrades to "row stored, vector NULL" when the
// embedding provider is down — reads and patches must never depend on OpenAI
// being up, only searchability may lag.

import { backfillRows, type BackfillBatch } from "../embeddings/backfill";
import type { EmbeddingService } from "../embeddings/service";
import { buildIndexText, chunkMarkdown, DEFAULT_CHUNK_CHARS } from "./projection";
import { docRef, factRef } from "./refs";
import type { IndexUpsert, MemoryStore } from "./store";
import type { Doc, Fact, Project } from "./types";

export interface Indexer {
  indexDoc(project: Project, doc: Doc): Promise<void>;
  indexFact(fact: Fact): Promise<void>;
  // One batch of the rows a failed inline embed left with a NULL vector,
  // past `afterId` in id order — see drainMissing.
  embedMissingBatch(batchSize?: number, afterId?: number): Promise<BackfillBatch>;
  // null when the provider is unreachable — recall turns that into an
  // explicit "search unavailable" rather than an empty result set.
  embedQuery(query: string): Promise<number[] | null>;
}

export interface IndexerDeps {
  store: MemoryStore;
  embeddings: EmbeddingService;
  chunkChars?: number;
}

export function createIndexer(deps: IndexerDeps): Indexer {
  const { store, embeddings } = deps;
  const chunkChars = deps.chunkChars ?? DEFAULT_CHUNK_CHARS;

  // One batch per source object. A failure is logged and swallowed: the rows
  // still land, with `embedding: null`, and the backfill picks them up.
  const embedTexts = async (texts: string[]): Promise<(number[] | null)[]> => {
    if (texts.length === 0) return [];
    try {
      const vectors = await embeddings.embedBatch(texts);
      return texts.map((_, i) => vectors[i] ?? null);
    } catch (err) {
      console.error(
        `[memory-index] embed failed for ${texts.length} chunk(s):`,
        err instanceof Error ? err.message : err,
      );
      return texts.map(() => null);
    }
  };

  return {
    indexDoc: async (project, doc) => {
      const chunks = chunkMarkdown(doc.body, chunkChars);
      const texts = chunks.map((chunk) =>
        buildIndexText({
          projectTitle: project.title,
          docName: doc.name,
          headingPath: chunk.headingPath,
          text: chunk.text,
        }),
      );
      const vectors = await embedTexts(texts);
      const entries: IndexUpsert[] = chunks.map((_, i) => ({
        sourceRef: docRef(project.slug, doc.name),
        ref: docRef(project.slug, doc.name, i),
        text: texts[i]!,
        tags: [],
        actor: null,
        // Documents have no lifecycle of their own; they are always current.
        state: "active",
        ts: doc.updatedAt,
        embedding: vectors[i] ?? null,
      }));
      // Always replace, even with zero entries: a document emptied of content
      // must not keep answering recalls from its old chunks.
      await store.replaceIndex(docRef(project.slug, doc.name), entries, { kind: "doc", docId: doc.id, version: doc.version });
    },

    indexFact: async (fact) => {
      const [vector] = await embedTexts([fact.body]);
      await store.replaceIndex(factRef(fact.id), [
        {
          sourceRef: factRef(fact.id),
          ref: factRef(fact.id),
          text: fact.body,
          tags: fact.tags,
          actor: fact.source,
          // Archiving a fact removes it from default recall without deleting
          // anything (D7).
          state: fact.state,
          ts: fact.updatedAt,
          embedding: vector ?? null,
        },
      ], { kind: "fact", id: fact.id, body: fact.body, tags: fact.tags, state: fact.state });
    },

    embedMissingBatch: async (batchSize = 100, afterId = 0) =>
      backfillRows(embeddings, await store.listUnembedded(batchSize, afterId), (row) => row.text, (row, vector) => store.setEmbedding(row.id, vector)),

    embedQuery: async (query) => {
      const [vector] = await embedTexts([query]);
      return vector ?? null;
    },
  };
}
