import { fetchArticleWithRetry, type ExtractedArticle } from "../../core/article";
import type { NewsProvider } from "../../core/provider";
import type { NewsItem } from "../../core/types";
import { fetchHackerNewsHeadlines, type HnHeadline } from "./api";

const SOURCE = "hackernews";
const DEFAULT_LIMIT = 30;
const DEFAULT_CADENCE_MS = 15 * 60_000;

export type HnHeadlineFetcher = (limit: number) => Promise<HnHeadline[]>;
export type ArticleFetcher = (url: string) => Promise<ExtractedArticle | null>;

export interface HackerNewsProviderDeps {
  fetchHeadlines: HnHeadlineFetcher;
  fetchArticle: ArticleFetcher;
}

export interface HackerNewsProviderOpts {
  limit?: number;
  cadenceMs?: number;
}

export function defaultHackerNewsDeps(): HackerNewsProviderDeps {
  return {
    fetchHeadlines: fetchHackerNewsHeadlines,
    fetchArticle: fetchArticleWithRetry,
  };
}

export function createHackerNewsProvider(
  deps: HackerNewsProviderDeps,
  opts: HackerNewsProviderOpts = {},
): NewsProvider {
  const limit = opts.limit ?? DEFAULT_LIMIT;
  const cadenceMs = opts.cadenceMs ?? DEFAULT_CADENCE_MS;

  return {
    source: SOURCE,
    cadenceMs,
    fetch: async (): Promise<NewsItem[]> => {
      const headlines = await deps.fetchHeadlines(limit);
      const items = await Promise.all(
        headlines.map(async (h): Promise<NewsItem | null> => {
          const article = await deps.fetchArticle(h.url);
          if (!article || !article.text.trim()) return null;
          return {
            source: SOURCE,
            externalId: h.url,
            title: article.title || h.title,
            url: h.url,
            body: article.text,
            metadata: {
              hn_id: h.id,
              score: h.score,
              comments: h.comments,
              author: h.author ?? article.author,
              site: article.site,
            },
            postedAt: article.publishedAt ?? h.postedAt,
          };
        }),
      );
      return items.filter((i): i is NewsItem => i !== null);
    },
  };
}
