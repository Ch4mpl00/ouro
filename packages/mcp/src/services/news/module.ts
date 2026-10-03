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
} from "./providers/hackernews/provider";
import { createHabrProvider, defaultHabrDeps } from "./providers/habr/provider";
import {
  createTelegramChannelsProvider,
  defaultTelegramChannelsDeps,
} from "./providers/telegram-channels/provider";
import { startNewsPoller } from "./poller";
import type { Userbot } from "../telegram/userbot";
import type { EmbeddingProvider } from "../embeddings/provider";
import { createGuardedFetch, type GuardedFetch } from "../fetch";

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
  // Harvests subscribed Telegram channels. Absent in CLIs that only need
  // the repository: no channel provider then.
  userbot?: Userbot;
  // Defaults to OpenAI; tests pass a deterministic one.
  embeddingProvider?: EmbeddingProvider;
  // Article pages go through the SSRF guard.
  fetcher?: GuardedFetch;
}

export function createNewsModule(deps: NewsModuleDeps): NewsModule {
  const { db } = deps;

  const embeddings = createEmbeddingService({
    provider: deps.embeddingProvider ?? getDefaultOpenAIProvider(),
    chunker: truncateChunker(),
  });
  const embedder = createNewsEmbedder({ db, embeddings });
  const repository = createNewsRepository({ db, embeddings, embedder });

  const fetcher = deps.fetcher ?? createGuardedFetch();
  const providers: NewsProvider[] = [
    createHackerNewsProvider(defaultHackerNewsDeps(fetcher)),
    createHabrProvider(defaultHabrDeps(fetcher)),
    ...(deps.userbot ? [createTelegramChannelsProvider(defaultTelegramChannelsDeps(db, deps.userbot))] : []),
  ];

  return { repository, providers };
}

export function startNewsModule(mod: NewsModule): void {
  startNewsPoller({ providers: mod.providers, repository: mod.repository });
}
