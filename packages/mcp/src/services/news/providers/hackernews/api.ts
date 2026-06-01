const API = "https://hacker-news.firebaseio.com/v0";

export interface HnHeadline {
  id: number;
  title: string;
  url: string;
  score?: number;
  comments?: number;
  author?: string;
  postedAt: Date | null;
}

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

export async function fetchHackerNewsHeadlines(limit: number): Promise<HnHeadline[]> {
  const idsRes = await fetch(`${API}/topstories.json`);
  if (!idsRes.ok) throw new Error(`HN topstories failed: ${idsRes.status}`);
  const ids = (await idsRes.json()) as number[];
  const top = ids.slice(0, limit);
  const items = await Promise.all(
    top.map(async (id): Promise<HnHeadline | null> => {
      try {
        const r = await fetch(`${API}/item/${id}.json`);
        if (!r.ok) return null;
        const d = (await r.json()) as HnItem;
        if (!d.title) return null;
        return {
          id,
          title: d.title,
          url: d.url ?? `https://news.ycombinator.com/item?id=${id}`,
          score: d.score,
          comments: d.descendants,
          author: d.by,
          postedAt: d.time ? new Date(d.time * 1000) : null,
        };
      } catch {
        return null;
      }
    }),
  );
  return items.filter((i): i is HnHeadline => i !== null);
}
