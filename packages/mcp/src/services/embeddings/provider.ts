import OpenAI from "openai";
import chunk from "lodash/chunk";

export interface EmbeddingProvider {
  readonly dimensions: number;
  embed(text: string): Promise<number[]>;
  embedBatch(texts: string[]): Promise<number[][]>;
}

// OpenAI accepts up to 2048 inputs per request; 100 keeps the payload modest.
const DEFAULT_BATCH_SIZE = 100;

export interface OpenAIProviderOpts {
  apiKey: string;
  model?: string;
  dimensions?: number;
  batchSize?: number;
}

export function createOpenAIProvider(opts: OpenAIProviderOpts): EmbeddingProvider {
  const client = new OpenAI({ apiKey: opts.apiKey });
  const model = opts.model ?? "text-embedding-3-small";
  const dimensions = opts.dimensions ?? 1536;
  const batchSize = opts.batchSize ?? DEFAULT_BATCH_SIZE;

  const embedBatch = async (texts: string[]): Promise<number[][]> => {
    if (texts.length === 0) return [];
    const batches = chunk(texts, batchSize);
    const results = await Promise.all(
      batches.map((input) => client.embeddings.create({ model, input, dimensions })),
    );
    return results.flatMap((r) => r.data.map((d) => d.embedding));
  };

  const embed = async (text: string): Promise<number[]> => {
    const [vector] = await embedBatch([text]);
    if (!vector) {
      throw new Error("OpenAI embeddings returned no vector for input");
    }
    return vector;
  };

  return { dimensions, embed, embedBatch };
}

// Shared OpenAI provider for modules that don't want to manage their
// own. Reads OPENAI_API_KEY from env on first access. One HTTP client
// per process — multiple modules can share it safely.
let sharedDefault: EmbeddingProvider | undefined;

export function getDefaultOpenAIProvider(): EmbeddingProvider {
  if (!sharedDefault) {
    const apiKey = process.env.OPENAI_API_KEY;
    if (!apiKey) {
      throw new Error(
        "OPENAI_API_KEY is not set. Embeddings need it; see .env.mcp.example.",
      );
    }
    sharedDefault = createOpenAIProvider({ apiKey });
  }
  return sharedDefault;
}
