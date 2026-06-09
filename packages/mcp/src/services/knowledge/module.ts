import type { Database } from "../../db/pg/client";
import { truncateChunker } from "../embeddings/chunker";
import { getDefaultOpenAIProvider } from "../embeddings/provider";
import { createEmbeddingService } from "../embeddings/service";
import {
  createKnowledgeRepository,
  type KnowledgeRepository,
} from "./repository";

// Composition root for the knowledge-base domain. Wires the shared
// embeddings provider + chunker into a KnowledgeRepository. No poller
// and no startKnowledgeModule — notes arrive synchronously through the
// add_note tool, there is nothing to poll.

export interface KnowledgeModule {
  repository: KnowledgeRepository;
}

export interface KnowledgeModuleDeps {
  db: Database;
}

export function createKnowledgeModule(
  deps: KnowledgeModuleDeps,
): KnowledgeModule {
  const { db } = deps;

  const embeddings = createEmbeddingService({
    provider: getDefaultOpenAIProvider(),
    chunker: truncateChunker(),
  });
  const repository = createKnowledgeRepository({ db, embeddings });

  return { repository };
}
