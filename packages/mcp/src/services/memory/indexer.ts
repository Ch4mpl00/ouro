// Keeps `memory_index` in step with the read models (D4/D8).
//
// The one background-ish concern in v1: chunk, denormalise, embed. It runs
// inline after each write and degrades to "row stored, vector NULL" when the
// embedding provider is down — reads and patches must never depend on OpenAI
// being up, only searchability may lag.

import type { EmbeddingService } from "../embeddings/service";
import { buildIndexText, chunkMarkdown, DEFAULT_CHUNK_CHARS } from "./projection";
import { docRef, factRef } from "./refs";
import type { IndexUpsert, MemoryStore } from "./store";
import type { Doc, Fact, Project } from "./types";

export interface EmbedResult {
  embedded: number;
  failed: number;
}

export interface Indexer {
  indexDoc(project: Project, doc: Doc): Promise<void>;
  indexFact(fact: Fact): Promise<void>;
  // Drains rows left with a NULL vector by a failed inline embed. Returns
  // 0/0 when the backlog is empty so a caller can loop until drained.
  embedMissingBatch(batchSize?: number): Promise<EmbedResult>;
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
      await store.replaceIndex(docRef(project.slug, doc.name), entries);
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
      ]);
    },

    embedMissingBatch: async (batchSize = 100) => {
      const rows = await store.listUnembedded(batchSize);
      if (rows.length === 0) return { embedded: 0, failed: 0 };
      const vectors = await embedTexts(rows.map((r) => r.text));
      let embedded = 0;
      for (const [i, row] of rows.entries()) {
        const vector = vectors[i];
        if (!vector) continue;
        await store.setEmbedding(row.id, vector);
        embedded++;
      }
      return { embedded, failed: rows.length - embedded };
    },

    embedQuery: async (query) => {
      const [vector] = await embedTexts([query]);
      return vector ?? null;
    },
  };
}
