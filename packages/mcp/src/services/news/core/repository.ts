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

export interface SearchManyOpts {
  // 1–N independent queries. Each is embedded and searched separately;
  // the union is merged (min distance wins per item) and de-duplicated
  // in embedding space across queries before the top-k cut. Filters and
  // k apply to the whole batch.
  queries: string[];
  k?: number;
  filter?: SearchFilter;
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
  // Indices into the `queries` batch that surfaced this item. Present
  // only on the batch path (searchMany); omitted for single-query search.
  matchedQueries?: number[];
}

export interface NewsRepository {
  save(items: NewsItem[]): Promise<SaveResult>;
  upsert(item: NewsItem): Promise<SaveResult>;
  findByExternalId(source: string, externalId: string): Promise<NewsItem | null>;
  list(opts: ListOpts): Promise<NewsItem[]>;
  search(opts: SearchOpts): Promise<SearchResult[]>;
  searchMany(opts: SearchManyOpts): Promise<SearchResult[]>;
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

    search: async (opts) =>
      searchCore(deps, [opts.query], {
        k: opts.k,
        filter: opts.filter,
        dedupThreshold: opts.dedupThreshold,
        // Single-query callers expect the original shape; suppress the
        // batch-only matchedQueries annotation.
        annotateMatches: false,
      }),

    searchMany: async (opts) =>
      searchCore(deps, opts.queries, {
        k: opts.k,
        filter: opts.filter,
        dedupThreshold: opts.dedupThreshold,
        annotateMatches: true,
      }),

    embedMissingBatch: embedder.embedMissingBatch,
  };
}

interface SearchCoreOpts {
  k?: number;
  filter?: SearchFilter;
  dedupThreshold?: number;
  annotateMatches: boolean;
}

// Shared retrieval core for both single (`search`) and batch
// (`searchMany`) paths. Embeds every query in one provider batch call,
// runs a per-query vector search (each keeps its own 2× pool), merges
// the union by news_items.id keeping the minimum distance, de-duplicates
// the merged set in embedding space, and returns the top-k by min
// distance. A single query is just the N=1 case — no cross merge, same
// behaviour as before.
async function searchCore(
  deps: NewsRepositoryDeps,
  queries: string[],
  opts: SearchCoreOpts,
): Promise<SearchResult[]> {
  const { db, embeddings } = deps;
  const k = opts.k ?? 10;
  const threshold = opts.dedupThreshold ?? DEFAULT_DEDUP_THRESHOLD;
  const dedupEnabled = threshold > 0;
  // 2× headroom over k (min 30) leaves room for near-duplicates to be
  // pruned while still leaving k survivors. Tuned on the RAG eval. Each
  // query keeps its own pool — the DB-side limit caps per-query traffic.
  const poolSize = dedupEnabled ? Math.max(k * 2, 30) : k;

  const filter = opts.filter ?? {};
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

  // One batch embed for the whole pack, then per-query searches in
  // parallel — N round trips to PG, but a single embed provider call.
  const vectors = await embeddings.embedBatch(queries);
  const pools = await Promise.all(
    vectors.map((queryVector) => {
      const distance = cosineDistance(newsItems.embedding, queryVector);
      return db
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
    }),
  );

  return mergeRankedPools(pools, {
    k,
    threshold,
    annotateMatches: opts.annotateMatches,
  });
}

// Minimal row shape the merge step needs — a structural subset of the
// drizzle search projection, so prod rows pass as-is and tests can hand
// in plain objects.
export interface RankablePoolRow {
  id: number;
  source: string;
  title: string | null;
  url: string | null;
  body: string;
  metadata: unknown;
  postedAt: Date | null;
  // The drizzle cosineDistance projection is typed `unknown` (pg returns
  // numeric as a string); coerced with Number() at merge time.
  distance: unknown;
  embedding: number[] | null;
}

// Pure merge + rank + dedup over per-query pools. Each pool is one
// query's rows pre-sorted by ascending distance. An item retrieved by
// several queries is kept once with its best (minimum) distance;
// `matchedQueries` records which facet indices surfaced it. The merged
// set is sorted by min distance, de-duplicated in embedding space across
// queries, then cut to the top-k. Separated from I/O so the ranking
// contract is unit-testable without a database.
export function mergeRankedPools(
  pools: RankablePoolRow[][],
  opts: { k: number; threshold: number; annotateMatches: boolean },
): SearchResult[] {
  const dedupEnabled = opts.threshold > 0;
  const byId = new Map<
    number,
    { row: RankablePoolRow; distance: number; matchedQueries: number[] }
  >();
  pools.forEach((rows, qi) => {
    for (const row of rows) {
      const id = Number(row.id);
      const dist = Number(row.distance);
      const existing = byId.get(id);
      if (existing) {
        existing.matchedQueries.push(qi);
        if (dist < existing.distance) {
          existing.distance = dist;
          existing.row = row;
        }
      } else {
        byId.set(id, { row, distance: dist, matchedQueries: [qi] });
      }
    }
  });

  const merged = [...byId.values()].sort((a, b) => a.distance - b.distance);
  const deduped = dedupEnabled
    ? dedupByPairwiseCosine(merged, (m) => m.row.embedding, opts.threshold)
    : merged;

  return deduped.slice(0, opts.k).map((m) => {
    const r = m.row;
    return {
      id: Number(r.id),
      source: r.source,
      title: r.title,
      url: r.url,
      snippet: r.body.length > SNIPPET_CHARS ? r.body.slice(0, SNIPPET_CHARS) + "…" : r.body,
      postedAt: r.postedAt?.toISOString() ?? null,
      distance: m.distance,
      metadata: (r.metadata ?? {}) as Record<string, unknown>,
      ...(opts.annotateMatches ? { matchedQueries: m.matchedQueries } : {}),
    };
  });
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
