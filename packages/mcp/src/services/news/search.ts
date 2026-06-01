import { and, cosineDistance, eq, gt, isNotNull, lte, or, sql } from "drizzle-orm";
import type { Database } from "../../db/pg/client";
import { newsItems } from "../../db/pg/schema";
import type { EmbeddingService } from "../embeddings/service";
import type { NewsSource } from "./types";

const SNIPPET_CHARS = 400;

export interface SearchFilter {
  source?: NewsSource | "channel";
  sinceISO?: string;
  untilISO?: string;
  chatId?: string; // applies only when source='channel'
}

export interface SearchOpts {
  query: string;
  k?: number;
  filter?: SearchFilter;
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

export interface SearchDeps {
  db: Database;
  embeddings: EmbeddingService;
}

export async function performSearch(
  opts: SearchOpts,
  deps: SearchDeps,
): Promise<SearchResult[]> {
  const k = opts.k ?? 10;
  const filter = opts.filter ?? {};

  const queryVector = await deps.embeddings.embedQuery(opts.query);

  const filters = [isNotNull(newsItems.embedding)];
  if (filter.source) filters.push(eq(newsItems.source, filter.source));
  if (filter.sinceISO) filters.push(gt(newsItems.postedAt, new Date(filter.sinceISO)));
  if (filter.untilISO) filters.push(lte(newsItems.postedAt, new Date(filter.untilISO)));
  if (filter.chatId) {
    const chatFilter = or(
      sql`${newsItems.metadata} ->> 'chat_username' = ${filter.chatId}`,
      sql`${newsItems.metadata} ->> 'chat_id' = ${filter.chatId}`,
    );
    if (chatFilter) filters.push(chatFilter);
  }

  const distance = cosineDistance(newsItems.embedding, queryVector);
  const rows = await deps.db
    .select({
      id: newsItems.id,
      source: newsItems.source,
      title: newsItems.title,
      url: newsItems.url,
      body: newsItems.body,
      metadata: newsItems.metadata,
      postedAt: newsItems.postedAt,
      distance,
    })
    .from(newsItems)
    .where(and(...filters))
    .orderBy(distance) // cosine distance ascending: closest neighbors first
    .limit(k);

  return rows.map((r) => ({
    id: Number(r.id),
    source: r.source,
    title: r.title,
    url: r.url,
    snippet: r.body.length > SNIPPET_CHARS ? r.body.slice(0, SNIPPET_CHARS) + "…" : r.body,
    postedAt: r.postedAt?.toISOString() ?? null,
    distance: Number(r.distance),
    metadata: (r.metadata ?? {}) as Record<string, unknown>,
  }));
}
