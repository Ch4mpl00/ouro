import Parser from "rss-parser";

// Habr exposes RSS at /ru/rss/all/ (overall feed). Per-hub feeds would
// be /ru/rss/hub/<slug>/, but we keep it simple — let downstream filter
// by interest from the unified stream.
const FEED_URL = "https://habr.com/ru/rss/all/";

const parser = new Parser({ timeout: 15_000 });

export interface HabrHeadline {
  title: string;
  url: string;
  author?: string;
  postedAt: Date | null;
}

export async function fetchHabrHeadlines(limit: number): Promise<HabrHeadline[]> {
  const feed = await parser.parseURL(FEED_URL);
  const out: HabrHeadline[] = [];
  for (const it of feed.items.slice(0, limit)) {
    if (!it.title || !it.link) continue;
    const ts = it.isoDate ?? it.pubDate;
    out.push({
      title: it.title,
      url: it.link,
      author: it.creator ?? (it as { author?: string }).author,
      postedAt: ts ? new Date(ts) : null,
    });
  }
  return out;
}
