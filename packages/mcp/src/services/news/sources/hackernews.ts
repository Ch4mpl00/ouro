import type { NewsItem } from "../types";

const API = "https://hacker-news.firebaseio.com/v0";

interface HnItem {
  id: number;
  title?: string;
  url?: string;
  score?: number;
  descendants?: number;
  by?: string;
  time?: number;
  type?: string;
}

export async function fetchHackerNews(limit = 30): Promise<NewsItem[]> {
  const idsRes = await fetch(`${API}/topstories.json`);
  if (!idsRes.ok) throw new Error(`HN topstories failed: ${idsRes.status}`);
  const ids = (await idsRes.json()) as number[];
  const top = ids.slice(0, limit);
  const items = await Promise.all(
    top.map(async (id): Promise<NewsItem | null> => {
      try {
        const r = await fetch(`${API}/item/${id}.json`);
        if (!r.ok) return null;
        const d = (await r.json()) as HnItem;
        if (!d.title) return null;
        return {
          source: "hackernews",
          title: d.title,
          url: d.url ?? `https://news.ycombinator.com/item?id=${id}`,
          score: d.score,
          comments: d.descendants,
          author: d.by,
          publishedAt: d.time ? new Date(d.time * 1000).toISOString() : undefined,
        };
      } catch {
        return null;
      }
    }),
  );
  return items.filter((i): i is NewsItem => i !== null);
}
