import type { Chunker } from "./chunker";
import type { EmbeddingProvider } from "./provider";

// Thin wrapper over provider + chunker: text in, vector out. No
// awareness of where vectors are stored or how text is composed —
// callers own that.
//
// The chunker is applied per input and only the first chunk is kept,
// keeping the 1:1 shape provider.embedBatch expects. A multi-chunk
// strategy belongs to a different API (it would need to return
// multiple vectors per input and the caller would need to store them).

export interface EmbeddingService {
  embed(text: string): Promise<number[]>;
  embedBatch(texts: string[]): Promise<number[][]>;
}

export interface EmbeddingServiceDeps {
  provider: EmbeddingProvider;
  chunker: Chunker;
}

export function createEmbeddingService(deps: EmbeddingServiceDeps): EmbeddingService {
  const { provider, chunker } = deps;

  const embed = (text: string): Promise<number[]> => {
    const [head = ""] = chunker(text);
    return provider.embed(head);
  };

  const embedBatch = (texts: string[]): Promise<number[][]> => {
    if (texts.length === 0) return Promise.resolve([]);
    const heads = texts.map((t) => chunker(t)[0] ?? "");
    return provider.embedBatch(heads);
  };

  return { embed, embedBatch };
}
