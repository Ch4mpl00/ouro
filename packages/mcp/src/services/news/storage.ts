import { and, eq } from "drizzle-orm";
import type { Database } from "../../db/pg/client";
import { newsItems } from "../../db/pg/schema";
import type { NewsItem, NewsSource } from "./types";
import type { ExtractedArticle } from "./article";

// HN/Habr rows in news_items use the article URL as external_id.

export interface NewsItemMetadata {
  score?: number;
  comments?: number;
  author?: string;
  site?: string;
}

export interface CachedArticle {
  id: number;
  source: NewsSource;
  externalId: string;
  title: string | null;
  url: string | null;
  body: string;
  metadata: NewsItemMetadata;
  postedAt: string | null;
}

export interface NewsStorage {
  // Returns the external_ids that were actually inserted (vs already
  // present), so the caller can embed only the new rows.
  upsertHeadlines(items: NewsItem[]): Promise<string[]>;
  getCachedArticle(source: NewsSource, url: string): Promise<CachedArticle | null>;
  // Caller pre-merges metadata: drizzle's set-object overwrites the
  // whole jsonb column, so headline-time keys (score/comments) would be
  // lost if we passed only the article-time fields here. Embedding is
  // cleared so the caller's re-embed step picks the row up.
  upsertArticleBody(opts: {
    source: NewsSource;
    url: string;
    article: ExtractedArticle;
    mergedMetadata: NewsItemMetadata;
  }): Promise<void>;
}

export function createNewsStorage(db: Database): NewsStorage {
  const upsertHeadlines = async (items: NewsItem[]): Promise<string[]> => {
    if (items.length === 0) return [];
    const values = items.map((i) => ({
      source: i.source,
      externalId: i.url,
      title: i.title,
      url: i.url,
      metadata: {
        score: i.score,
        comments: i.comments,
        author: i.author,
      } satisfies NewsItemMetadata,
      postedAt: i.publishedAt ? new Date(i.publishedAt) : null,
    }));
    const inserted = await db
      .insert(newsItems)
      .values(values)
      .onConflictDoNothing({ target: [newsItems.source, newsItems.externalId] })
      .returning({ externalId: newsItems.externalId });
    return inserted.map((r) => r.externalId);
  };

  const getCachedArticle = async (
    source: NewsSource,
    url: string,
  ): Promise<CachedArticle | null> => {
    const rows = await db
      .select({
        id: newsItems.id,
        source: newsItems.source,
        externalId: newsItems.externalId,
        title: newsItems.title,
        url: newsItems.url,
        body: newsItems.body,
        metadata: newsItems.metadata,
        postedAt: newsItems.postedAt,
      })
      .from(newsItems)
      .where(and(eq(newsItems.source, source), eq(newsItems.externalId, url)))
      .limit(1);
    const r = rows[0];
    if (!r) return null;
    return {
      id: Number(r.id),
      source: r.source as NewsSource,
      externalId: r.externalId,
      title: r.title,
      url: r.url,
      body: r.body,
      metadata: (r.metadata ?? {}) as NewsItemMetadata,
      postedAt: r.postedAt?.toISOString() ?? null,
    };
  };

  const upsertArticleBody = async (opts: {
    source: NewsSource;
    url: string;
    article: ExtractedArticle;
    mergedMetadata: NewsItemMetadata;
  }): Promise<void> => {
    await db
      .insert(newsItems)
      .values({
        source: opts.source,
        externalId: opts.url,
        title: opts.article.title || null,
        url: opts.url,
        body: opts.article.text,
        metadata: opts.mergedMetadata,
        postedAt: opts.article.publishedAt
          ? new Date(opts.article.publishedAt)
          : null,
      })
      .onConflictDoUpdate({
        target: [newsItems.source, newsItems.externalId],
        set: {
          title: opts.article.title || null,
          body: opts.article.text,
          metadata: opts.mergedMetadata,
          embedding: null,
          embeddedAt: null,
        },
      });
  };

  return { upsertHeadlines, getCachedArticle, upsertArticleBody };
}
