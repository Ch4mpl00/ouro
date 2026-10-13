import { and, asc, eq, gt, isNull } from "drizzle-orm";
import type { Database } from "../../../db/pg/client";
import { newsItems } from "../../../db/pg/schema";
import { backfillRows, type BackfillBatch } from "../../embeddings/backfill";
import type { EmbeddingService } from "../../embeddings/service";

// Takes rows already in hand (from a fresh insert) or pulls
// embedding-less rows from storage. Composes embedding text from
// title + body, runs through the generic EmbeddingService, writes
// vectors back. Source-agnostic.

export interface EmbedRow {
  id: number;
  title: string | null;
  body: string;
}

export interface EmbedResult {
  embedded: number;
  failed: number;
}

export interface NewsEmbedder {
  embed(rows: EmbedRow[]): Promise<EmbedResult>;
  // Picks up rows with embedding=NULL (left behind by a previous failed
  // inline embed) past `afterId`, in id order, and embeds one batch — see
  // drainMissing.
  embedMissingBatch(batchSize?: number, afterId?: number): Promise<BackfillBatch>;
}

export interface NewsEmbedderDeps {
  db: Database;
  embeddings: EmbeddingService;
}

export function createNewsEmbedder(deps: NewsEmbedderDeps): NewsEmbedder {
  const { db, embeddings } = deps;

  const buildText = (row: EmbedRow): string => {
    return [
      (row.title ?? "").trim(),
      row.body.trim()
    ].filter(Boolean).join("\n\n");
  };

  const embed = async (rows: EmbedRow[]): Promise<EmbedResult> => {
    if (rows.length === 0) return { embedded: 0, failed: 0 };
    const texts = rows.map(buildText);
    let vectors: number[][];
    try {
      vectors = await embeddings.embedBatch(texts);
    } catch (err) {
      console.error(
        `[news-embedder] embed failed for ${rows.length} rows:`,
        err instanceof Error ? err.message : err,
      );
      return { embedded: 0, failed: rows.length };
    }
    let embedded = 0;
    for (let i = 0; i < rows.length; i++) {
      const row = rows[i]!;
      const vector = vectors[i];
      if (!vector) continue;
      await db
        .update(newsItems)
        .set({ embedding: vector, embeddedAt: new Date() })
        .where(eq(newsItems.id, row.id));
      embedded++;
    }
    return { embedded, failed: 0 };
  };

  const embedMissingBatch = async (batchSize = 100, afterId = 0): Promise<BackfillBatch> => {
    const rows = await db
      .select({
        id: newsItems.id,
        title: newsItems.title,
        body: newsItems.body,
      })
      .from(newsItems)
      .where(and(isNull(newsItems.embedding), gt(newsItems.id, afterId)))
      .orderBy(asc(newsItems.id))
      .limit(batchSize);
    return backfillRows(
      embeddings,
      rows.map((r) => ({ ...r, id: Number(r.id) })),
      buildText,
      async (row, vector) => {
        await db.update(newsItems).set({ embedding: vector, embeddedAt: new Date() }).where(eq(newsItems.id, row.id));
      },
    );
  };

  return { embed, embedMissingBatch };
}
