import { and, eq, inArray } from "drizzle-orm";
import type { Database } from "../../db/pg/client";
import { newsItems } from "../../db/pg/schema";
import type { EmbeddableRow, EmbeddingRepository } from "../embeddings/repository";

// Implementation of EmbeddingRepository backed by news_items. Lives in
// the news module because that's where the table is owned; the
// embeddings module sees only the interface.

export function createNewsItemsRepository(db: Database): EmbeddingRepository {
  // drizzle's inArray is single-column, so we group targets by source
  // and run one query per source.
  const findByCompositeKeys = async (
    targets: { source: string; externalId: string }[],
  ): Promise<EmbeddableRow[]> => {
    if (targets.length === 0) return [];
    const bySource = new Map<string, string[]>();
    for (const t of targets) {
      const list = bySource.get(t.source) ?? [];
      list.push(t.externalId);
      bySource.set(t.source, list);
    }
    const out: EmbeddableRow[] = [];
    for (const [source, externalIds] of bySource) {
      const rows = await db
        .select({
          id: newsItems.id,
          source: newsItems.source,
          externalId: newsItems.externalId,
          title: newsItems.title,
          body: newsItems.body,
          embedding: newsItems.embedding,
        })
        .from(newsItems)
        .where(and(eq(newsItems.source, source), inArray(newsItems.externalId, externalIds)));
      for (const r of rows) out.push({ ...r, id: Number(r.id) });
    }
    return out;
  };

  const findByIds = async (ids: number[]): Promise<EmbeddableRow[]> => {
    if (ids.length === 0) return [];
    const rows = await db
      .select({
        id: newsItems.id,
        source: newsItems.source,
        externalId: newsItems.externalId,
        title: newsItems.title,
        body: newsItems.body,
        embedding: newsItems.embedding,
      })
      .from(newsItems)
      .where(inArray(newsItems.id, ids));
    return rows.map((r) => ({ ...r, id: Number(r.id) }));
  };

  const saveEmbedding = async (id: number, vector: number[]): Promise<void> => {
    await db
      .update(newsItems)
      .set({ embedding: vector, embeddedAt: new Date() })
      .where(eq(newsItems.id, id));
  };

  return { findByCompositeKeys, findByIds, saveEmbedding };
}
