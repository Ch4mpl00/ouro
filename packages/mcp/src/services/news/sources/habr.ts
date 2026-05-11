import Parser from "rss-parser";
import type { NewsItem } from "../types";

// Habr exposes RSS at /ru/rss/all/ (overall feed). Per-hub feeds would be
// /ru/rss/hub/<slug>/, but we keep it simple and let LLM filter by interest
// from the unified stream.
const FEED_URL = "https://habr.com/ru/rss/all/";

const parser = new Parser({ timeout: 15_000 });

export async function fetchHabr(limit = 30): Promise<NewsItem[]> {
  const feed = await parser.parseURL(FEED_URL);
  const out: NewsItem[] = [];
  for (const it of feed.items.slice(0, limit)) {
    if (!it.title || !it.link) continue;
    out.push({
      source: "habr",
      title: it.title,
      url: it.link,
      author: it.creator ?? (it as { author?: string }).author,
      publishedAt: it.isoDate ?? it.pubDate,
    });
  }
  return out;
}
