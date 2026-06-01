import type { Database } from "../../db/pg/client";
import { createEmbeddingsModule } from "../embeddings/module";
import type { EmbeddingService } from "../embeddings/service";
import { createNewsItemsRepository } from "./embedding-repository";
import { createNewsStorage, type NewsStorage } from "./storage";
import { performSearch, type SearchOpts, type SearchResult } from "./search";

// Domain module for news. Owns its EmbeddingService (configured against
// news_items), the article-side storage (HN/Habr), and the search
// entry point. Instantiated once in the composition root; consumers
// receive the assembled NewsModule via DI.

export interface NewsModule {
  embeddings: EmbeddingService;
  storage: NewsStorage;
  search(opts: SearchOpts): Promise<SearchResult[]>;
}

export interface NewsModuleDeps {
  db: Database;
}

export function createNewsModule(deps: NewsModuleDeps): NewsModule {
  const { db } = deps;
  const embeddings = createEmbeddingsModule({ repo: createNewsItemsRepository(db) });
  const storage = createNewsStorage(db);
  return {
    embeddings,
    storage,
    search: (opts) => performSearch(opts, { db, embeddings }),
  };
}
