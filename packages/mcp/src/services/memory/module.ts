// Composition root for unified memory. Wires the PG store and the shared
// embeddings provider into the service the tools call.

import type { Database } from "../../db/pg/client";
import { truncateChunker } from "../embeddings/chunker";
import { getDefaultOpenAIProvider } from "../embeddings/provider";
import { createEmbeddingService } from "../embeddings/service";
import { createIndexer, type Indexer } from "./indexer";
import { createMemoryService, type MemoryService } from "./service";
import { createPgMemoryStore } from "./store.pg";

export interface MemoryModule {
  service: MemoryService;
  // Exposed for the backfill script; nothing else should reach past `service`.
  indexer: Indexer;
}

export interface MemoryModuleDeps {
  db: Database;
}

export function createMemoryModule(deps: MemoryModuleDeps): MemoryModule {
  const store = createPgMemoryStore({ db: deps.db });
  // The chunker here only guards the embedder's token limit — `chunkMarkdown`
  // in projection.ts already decided the granularity.
  const embeddings = createEmbeddingService({
    provider: getDefaultOpenAIProvider(),
    chunker: truncateChunker(),
  });
  const indexer = createIndexer({ store, embeddings });
  const service = createMemoryService({ store, indexer });

  return { service, indexer };
}
