import { fetchHackerNews } from "./hackernews";
import { fetchHabr } from "./habr";
import type { NewsItem, NewsSource } from "../types";

const FETCHERS: Record<NewsSource, (limit: number) => Promise<NewsItem[]>> = {
  hackernews: fetchHackerNews,
  habr: fetchHabr,
};

export const ALL_SOURCES: NewsSource[] = ["hackernews", "habr"];

export async function fetchHeadlines(opts: {
  source?: NewsSource;
  limit?: number;
}): Promise<NewsItem[]> {
  const limit = opts.limit ?? 30;
  const sources: NewsSource[] = opts.source ? [opts.source] : ALL_SOURCES;
  const results = await Promise.all(
    sources.map(async (s) => {
      try {
        return await FETCHERS[s](limit);
      } catch (err) {
        console.error(`[news] ${s} fetch failed:`, err instanceof Error ? err.message : err);
        return [];
      }
    }),
  );
  return results.flat();
}
