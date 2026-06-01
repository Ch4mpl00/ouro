import { fetchArticleWithRetry, type ExtractedArticle } from "../../core/article";
import type { NewsProvider } from "../../core/provider";
import type { NewsItem } from "../../core/types";
import { fetchHabrHeadlines, type HabrHeadline } from "./api";

const SOURCE = "habr";
const DEFAULT_LIMIT = 30;
const DEFAULT_CADENCE_MS = 30 * 60_000;

export type HabrHeadlineFetcher = (limit: number) => Promise<HabrHeadline[]>;
export type ArticleFetcher = (url: string) => Promise<ExtractedArticle | null>;

export interface HabrProviderDeps {
  fetchHeadlines: HabrHeadlineFetcher;
  fetchArticle: ArticleFetcher;
}

export interface HabrProviderOpts {
  limit?: number;
  cadenceMs?: number;
}

export function defaultHabrDeps(): HabrProviderDeps {
  return {
    fetchHeadlines: fetchHabrHeadlines,
    fetchArticle: fetchArticleWithRetry,
  };
}

export function createHabrProvider(
  deps: HabrProviderDeps,
  opts: HabrProviderOpts = {},
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
              author: h.author ?? article.author,
              site: article.site,
            },
            postedAt: article.publishedAt ? new Date(article.publishedAt) : h.postedAt,
          };
        }),
      );
      return items.filter((i): i is NewsItem => i !== null);
    },
  };
}
