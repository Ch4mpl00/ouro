import type { Chunker } from "./chunker";
import type { EmbeddingProvider } from "./provider";
import type { EmbeddableRow, EmbeddingRepository } from "./repository";

// The service assumes a 1:1 chunk-to-row mapping (one embedding per
// news_items row). buildText guards against a multi-chunk chunker
// being plugged in without a corresponding schema change.

export interface EmbedResult {
  embedded: number;
  skipped: number;
  failed: number;
}

export interface EmbedTarget {
  source: string;
  externalId: string;
}

export interface EmbeddingService {
  embedQuery(query: string): Promise<number[]>;
  embedByTargets(targets: EmbedTarget[], opts?: { force?: boolean }): Promise<EmbedResult>;
  embedByIds(ids: number[]): Promise<EmbedResult>;
}

export interface EmbeddingServiceDeps {
  provider: EmbeddingProvider;
  chunker: Chunker;
  repo: EmbeddingRepository;
}

export function createEmbeddingService(deps: EmbeddingServiceDeps): EmbeddingService {
  const { provider, chunker, repo } = deps;

  const buildText = (row: EmbeddableRow): string => {
    const title = (row.title ?? "").trim();
    const body = row.body.trim();
    const combined = title && body ? `${title}\n\n${body}` : title || body;
    const chunks = chunker(combined);
    if (chunks.length > 1) {
      throw new Error(
        "embedding service received a multi-chunk Chunker, but the news_items " +
          "schema stores exactly one embedding per row. Introduce a " +
          "news_item_chunks table before swapping in a multi-chunk chunker.",
      );
    }
    return chunks[0] ?? "";
  };

  // Throws on provider failure: query-time errors surface to the
  // caller, unlike the inline-embed paths below which swallow into
  // failed=N so the rest of the tick stays unaffected.
  const embedQuery = async (query: string): Promise<number[]> => {
    const [head = ""] = chunker(query);
    return provider.embed(head);
  };

  const embedRows = async (rows: EmbeddableRow[], force: boolean): Promise<EmbedResult> => {
    const candidates = force ? rows : rows.filter((r) => r.embedding === null);
    const skipped = rows.length - candidates.length;
    if (candidates.length === 0) return { embedded: 0, skipped, failed: 0 };

    const texts = candidates.map(buildText);
    let vectors: number[][];
    try {
      vectors = await provider.embedBatch(texts);
    } catch (err) {
      console.error(
        `[embeddings] provider call failed for ${candidates.length} rows:`,
        err instanceof Error ? err.message : err,
      );
      return { embedded: 0, skipped, failed: candidates.length };
    }

    let embedded = 0;
    for (let i = 0; i < candidates.length; i++) {
      const row = candidates[i]!;
      const vector = vectors[i];
      if (!vector) continue;
      await repo.saveEmbedding(row.id, vector);
      embedded++;
    }
    return { embedded, skipped, failed: 0 };
  };

  // `force` re-embeds rows that already have a vector — for callers
  // (e.g. fetch_article) that just replaced the body.
  const embedByTargets = async (
    targets: EmbedTarget[],
    opts: { force?: boolean } = {},
  ): Promise<EmbedResult> => {
    if (targets.length === 0) return zero();
    const rows = await repo.findByCompositeKeys(targets);
    return embedRows(rows, opts.force ?? false);
  };

  // Treated as forced — the caller already filtered on embedding IS NULL.
  const embedByIds = async (ids: number[]): Promise<EmbedResult> => {
    if (ids.length === 0) return zero();
    const rows = await repo.findByIds(ids);
    return embedRows(rows, true);
  };

  return { embedQuery, embedByTargets, embedByIds };
}

function zero(): EmbedResult {
  return { embedded: 0, skipped: 0, failed: 0 };
}
