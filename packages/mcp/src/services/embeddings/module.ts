import { truncateChunker, type Chunker } from "./chunker";
import { getDefaultOpenAIProvider, type EmbeddingProvider } from "./provider";
import type { EmbeddingRepository } from "./repository";
import { createEmbeddingService, type EmbeddingService } from "./service";

// Module entry point. A consuming domain (news, agent-memory, …) calls
// this with its own repo to get its own EmbeddingService. The OpenAI
// provider defaults to a process-shared singleton; pass `provider` to
// override (different model, separate rate budget, fake for tests).

export interface EmbeddingsModuleConfig {
  repo: EmbeddingRepository;
  provider?: EmbeddingProvider;
  chunker?: Chunker;
}

export function createEmbeddingsModule(config: EmbeddingsModuleConfig): EmbeddingService {
  return createEmbeddingService({
    provider: config.provider ?? getDefaultOpenAIProvider(),
    chunker: config.chunker ?? truncateChunker(),
    repo: config.repo,
  });
}
