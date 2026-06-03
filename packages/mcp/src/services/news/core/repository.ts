import {
  and,
  asc,
  cosineDistance,
  desc,
  eq,
  gt,
  isNotNull,
  lte,
  or,
  sql,
} from "drizzle-orm";
import type { Database } from "../../../db/pg/client";
import { newsItems } from "../../../db/pg/schema";
import type { EmbeddingService } from "../../embeddings/service";
import {
  DEFAULT_DEDUP_THRESHOLD,
  dedupByPairwiseCosine,
} from "../../retrieval";
import type { EmbedResult, NewsEmbedder } from "./embedder";
import type { ListOpts, NewsItem, SaveResult } from "./types";

// The single facade the rest of the codebase sees. Items go in →
// stored → embedded → searchable. All drizzle queries live here; no
// separate storage/search layers — they were one-line wrappers each.

const SNIPPET_CHARS = 400;

export interface SearchFilter {
  source?: string;
  sinceISO?: string;
  untilISO?: string;
  // Channel-specific: matches metadata.chat_username OR chat_id.
  channel?: string;
}

export interface SearchOpts {
  query: string;
  k?: number;
  filter?: SearchFilter;
  // Near-duplicate filtering at retrieval. Defaults to
  // DEFAULT_DEDUP_THRESHOLD. Pass 0 to disable (e.g. for debugging).
  dedupThreshold?: number;
}

export interface SearchResult {
  id: number;
  source: string;
  title: string | null;
  url: string | null;
  snippet: string;
  postedAt: string | null;
  distance: number;
  metadata: Record<string, unknown>;
}

export interface NewsRepository {
  save(items: NewsItem[]): Promise<SaveResult>;
  upsert(item: NewsItem): Promise<SaveResult>;
  findByExternalId(source: string, externalId: string): Promise<NewsItem | null>;
  list(opts: ListOpts): Promise<NewsItem[]>;
  search(opts: SearchOpts): Promise<SearchResult[]>;
  embedMissingBatch(batchSize?: number): Promise<EmbedResult>;
}

export interface NewsRepositoryDeps {
  db: Database;
  embeddings: EmbeddingService;
  embedder: NewsEmbedder;
}

export function createNewsRepository(deps: NewsRepositoryDeps): NewsRepository {
  const { db, embeddings, embedder } = deps;

  return {
    save: async (items) => {
      if (items.length === 0) return { saved: 0, embedded: 0, failed: 0 };
      const inserted = await db
        .insert(newsItems)
        .values(
          items.map((i) => ({
            source: i.source,
            externalId: i.externalId,
            title: i.title,
            url: i.url,
            body: i.body,
            metadata: i.metadata,
            postedAt: i.postedAt,
          })),
        )
        .onConflictDoNothing({ target: [newsItems.source, newsItems.externalId] })
        .returning({
          id: newsItems.id,
          title: newsItems.title,
          body: newsItems.body,
        });
      if (inserted.length === 0) return { saved: 0, embedded: 0, failed: 0 };
      const rows = inserted.map((r) => ({ ...r, id: Number(r.id) }));
      const result = await embedder.embed(rows);
      return { saved: rows.length, embedded: result.embedded, failed: result.failed };
    },

    // Insert-or-update by (source, externalId). On conflict replaces
    // body/title/metadata and clears embedding so it gets re-embedded.
    upsert: async (item) => {
      const [inserted] = await db
        .insert(newsItems)
        .values({
          source: item.source,
          externalId: item.externalId,
          title: item.title,
          url: item.url,
          body: item.body,
          metadata: item.metadata,
          postedAt: item.postedAt,
        })
        .onConflictDoUpdate({
          target: [newsItems.source, newsItems.externalId],
          set: {
            title: item.title,
            body: item.body,
            metadata: item.metadata,
            embedding: null,
            embeddedAt: null,
          },
        })
        .returning({
          id: newsItems.id,
          title: newsItems.title,
          body: newsItems.body,
        });
      if (!inserted) throw new Error("upsert returned no row");
      const row = { ...inserted, id: Number(inserted.id) };
      const result = await embedder.embed([row]);
      return { saved: 1, embedded: result.embedded, failed: result.failed };
    },

    findByExternalId: async (source, externalId) => {
      const rows = await db
        .select()
        .from(newsItems)
        .where(and(eq(newsItems.source, source), eq(newsItems.externalId, externalId)))
        .limit(1);
      const r = rows[0];
      return r ? toNewsItem(r) : null;
    },

    list: async (opts) => {
      const limit = opts.limit ?? 500;
      const threshold = opts.dedupThreshold ?? DEFAULT_DEDUP_THRESHOLD;
      const dedupEnabled = threshold > 0;
      // When dedup is on, pull a 2× pool (capped at the table limit by
      // the same limit() call) so post-dedup we can still return up to
      // `limit` survivors. The DB-side LIMIT keeps the upper bound on
      // traffic; nothing is unbounded.
      const fetchLimit = dedupEnabled ? Math.min(limit * 2, 2000) : limit;

      const filters = [];
      if (opts.source) filters.push(eq(newsItems.source, opts.source));
      if (opts.sinceISO) filters.push(gt(newsItems.postedAt, new Date(opts.sinceISO)));
      if (opts.untilISO) filters.push(lte(newsItems.postedAt, new Date(opts.untilISO)));
      if (opts.channel) {
        const f = or(
          sql`${newsItems.metadata} ->> 'chat_username' = ${opts.channel}`,
          sql`${newsItems.metadata} ->> 'chat_id' = ${opts.channel}`,
        );
        if (f) filters.push(f);
      }
      const rows = await db
        .select()
        .from(newsItems)
        .where(filters.length > 0 ? and(...filters) : undefined)
        .orderBy(opts.sinceISO ? asc(newsItems.postedAt) : desc(newsItems.postedAt))
        .limit(fetchLimit);

      const deduped = dedupEnabled
        ? dedupByPairwiseCosine(rows, (r) => r.embedding, threshold, {
            keepNullVectors: true,
          })
        : rows;

      return deduped.slice(0, limit).map(toNewsItem);
    },

    search: async (opts) => {
      const k = opts.k ?? 10;
      const threshold = opts.dedupThreshold ?? DEFAULT_DEDUP_THRESHOLD;
      const dedupEnabled = threshold > 0;
      // 2× headroom over k (min 30) leaves room for near-duplicates to be
      // pruned while still leaving k survivors. Tuned on the RAG eval.
      const poolSize = dedupEnabled ? Math.max(k * 2, 30) : k;

      const filter = opts.filter ?? {};
      const queryVector = await embeddings.embed(opts.query);

      const filters = [isNotNull(newsItems.embedding)];
      if (filter.source) filters.push(eq(newsItems.source, filter.source));
      if (filter.sinceISO) filters.push(gt(newsItems.postedAt, new Date(filter.sinceISO)));
      if (filter.untilISO) filters.push(lte(newsItems.postedAt, new Date(filter.untilISO)));
      if (filter.channel) {
        const f = or(
          sql`${newsItems.metadata} ->> 'chat_username' = ${filter.channel}`,
          sql`${newsItems.metadata} ->> 'chat_id' = ${filter.channel}`,
        );
        if (f) filters.push(f);
      }

      const distance = cosineDistance(newsItems.embedding, queryVector);
      const rows = await db
        .select({
          id: newsItems.id,
          source: newsItems.source,
          title: newsItems.title,
          url: newsItems.url,
          body: newsItems.body,
          metadata: newsItems.metadata,
          postedAt: newsItems.postedAt,
          distance,
          embedding: newsItems.embedding,
        })
        .from(newsItems)
        .where(and(...filters))
        .orderBy(distance)
        .limit(poolSize);

      const deduped = dedupEnabled
        ? dedupByPairwiseCosine(rows, (r) => r.embedding, threshold)
        : rows;

      return deduped.slice(0, k).map((r) => ({
        id: Number(r.id),
        source: r.source,
        title: r.title,
        url: r.url,
        snippet: r.body.length > SNIPPET_CHARS ? r.body.slice(0, SNIPPET_CHARS) + "…" : r.body,
        postedAt: r.postedAt?.toISOString() ?? null,
        distance: Number(r.distance),
        metadata: (r.metadata ?? {}) as Record<string, unknown>,
      }));
    },

    embedMissingBatch: embedder.embedMissingBatch,
  };
}

function toNewsItem(r: typeof newsItems.$inferSelect): NewsItem {
  return {
    source: r.source,
    externalId: r.externalId,
    title: r.title,
    url: r.url,
    body: r.body,
    metadata: (r.metadata ?? {}) as Record<string, unknown>,
    postedAt: r.postedAt,
  };
}
