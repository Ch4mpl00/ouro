import type { Database } from "../../db/pg/client";
import { truncateChunker } from "../embeddings/chunker";
import { getDefaultOpenAIProvider } from "../embeddings/provider";
import { createEmbeddingService } from "../embeddings/service";
import { createNewsEmbedder } from "./core/embedder";
import type { NewsProvider } from "./core/provider";
import { createNewsRepository, type NewsRepository } from "./core/repository";
import {
  createHackerNewsProvider,
  defaultHackerNewsDeps,
} from "./providers/hackernews";
import { createHabrProvider, defaultHabrDeps } from "./providers/habr";
import {
  createTelegramChannelsProvider,
  defaultTelegramChannelsDeps,
} from "./providers/telegram-channels";
import { startNewsPoller } from "./poller";

// Composition root for the news domain. Wires embeddings + embedder
// into a NewsRepository, instantiates the default providers with their
// default deps, starts the shared poller. Tests can build their own
// composition using non-default deps and skip startNewsModule.

export interface NewsModule {
  repository: NewsRepository;
  providers: NewsProvider[];
}

export interface NewsModuleDeps {
  db: Database;
}

export function createNewsModule(deps: NewsModuleDeps): NewsModule {
  const { db } = deps;

  const embeddings = createEmbeddingService({
    provider: getDefaultOpenAIProvider(),
    chunker: truncateChunker(),
  });
  const embedder = createNewsEmbedder({ db, embeddings });
  const repository = createNewsRepository({ db, embeddings, embedder });

  const providers: NewsProvider[] = [
    createHackerNewsProvider(defaultHackerNewsDeps()),
    createHabrProvider(defaultHabrDeps()),
    createTelegramChannelsProvider(defaultTelegramChannelsDeps(db)),
  ];

  return { repository, providers };
}

export function startNewsModule(mod: NewsModule): void {
  startNewsPoller({ providers: mod.providers, repository: mod.repository });
}
