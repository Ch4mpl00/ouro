import { z } from "zod";
import type { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { fetchArticle } from "../services/news";
import type { NewsItem, NewsRepository } from "../services/news";
import { jsonResult } from "../result";

const KNOWN_SOURCES = ["hackernews", "habr", "channel"] as const;

// Contiguous even split into EXACTLY n parts (last parts may be shorter or
// empty — the caller promised the planner a fixed chunk count, so we always
// deliver n arrays). Contiguous, not round-robin: list_news is time-ordered
// and search results are relevance-ordered, so neighbouring items are the
// ones most likely to describe the same event — keeping them in one chunk
// lets a map-phase composer consolidate locally.
export function splitChunks<T>(items: readonly T[], n: number): T[][] {
  const out: T[][] = [];
  const base = Math.floor(items.length / n);
  const extra = items.length % n;
  let offset = 0;
  for (let i = 0; i < n; i++) {
    const size = base + (i < extra ? 1 : 0);
    out.push(items.slice(offset, offset + size));
    offset += size;
  }
  return out;
}

const CHUNKS_DESCRIPTION =
  "Map-reduce mode: split the result into EXACTLY this many contiguous " +
  "chunks and return { count, chunks: [...] } instead of a flat list — one " +
  "chunk per parallel map step. The chunk count is fixed so a workflow can " +
  "reference ${bind.chunks.0}, ${bind.chunks.1}, … statically; trailing " +
  "chunks may be empty when there are few items.";

export function registerNewsTools(server: McpServer, news: NewsRepository): void {
  server.registerTool(
    "search_news",
    {
      title: "Semantic search over the news store",
      description:
        "Vector search across every news item the pollers have ingested " +
        "(Hacker News, Habr, harvested Telegram channels). Returns the " +
        "closest matches by semantic similarity. Use this when the user " +
        "asks about a topic — the background pollers keep the store fresh, " +
        "so there is no need to fetch articles before searching. Returns " +
        "id, source, title, url, snippet (first ~400 chars of the body), " +
        "posted_at, distance (lower = closer), and source-specific metadata.\n\n" +
        "For a multi-facet ask (e.g. one topic spanning several distinct " +
        "subjects), pass `queries: [...]` — one entry per facet — instead " +
        "of calling this tool N times or blurring everything into one " +
        "`query`. Each query is searched independently; results are merged " +
        "and already de-duplicated across the batch (an item's `distance` " +
        "is its best match across the facets, `matchedQueries` lists which " +
        "facets surfaced it), so do NOT re-query per facet or re-dedup. " +
        "Pass exactly one of `query` or `queries`.",
      inputSchema: {
        query: z
          .string()
          .min(1)
          .optional()
          .describe("Natural-language search query. Use for a single facet."),
        queries: z
          .array(z.string().min(1))
          .min(1)
          .max(8)
          .optional()
          .describe(
            "Batch of 1–8 independent queries for a multi-facet ask. " +
              "Mutually exclusive with `query`.",
          ),
        k: z
          .number()
          .int()
          .min(1)
          .max(50)
          .optional()
          .describe("Number of results to return. Default 10."),
        source: z
          .enum(KNOWN_SOURCES)
          .optional()
          .describe("Restrict results to one source."),
        sinceISO: z
          .string()
          .optional()
          .describe("Only items with posted_at > this ISO timestamp."),
        untilISO: z
          .string()
          .optional()
          .describe("Only items with posted_at <= this ISO timestamp."),
        asOfISO: z
          .string()
          .optional()
          .describe(
            "Eval/judge point-in-time replay: only items already searchable " +
              "at this ISO instant (embedded_at <= asOfISO). Reconstructs " +
              "what search could have returned at a past moment, excluding " +
              "rows embedded later (poller/backfill lag). Normal runs omit this.",
          ),
        channel: z
          .string()
          .optional()
          .describe(
            "For source='channel' only: restrict to one Telegram channel " +
              "by chat_username or chat_id.",
          ),
        chunks: z.number().int().min(2).max(8).optional().describe(CHUNKS_DESCRIPTION),
      },
    },
    async ({ query, queries, k, source, sinceISO, untilISO, asOfISO, channel, chunks }) => {
      if ((query === undefined) === (queries === undefined)) {
        return jsonResult({
          error: "Pass exactly one of `query` or `queries`.",
        });
      }
      const filter = { source, sinceISO, untilISO, asOfISO, channel };
      const results = query
        ? await news.search({ query, k, filter })
        : await news.searchMany({ queries: queries ?? [], k, filter });
      if (chunks !== undefined) {
        return jsonResult({ count: results.length, chunks: splitChunks(results, chunks) });
      }
      return jsonResult({ count: results.length, results });
    },
  );

  server.registerTool(
    "list_news",
    {
      title: "List news items chronologically",
      description:
        "Read items from the news store ordered by posted_at. Use when " +
        "you need everything in a time window (e.g. a 24h channel digest) " +
        "rather than a topical match. Ascending when sinceISO is provided, " +
        "descending otherwise.",
      inputSchema: {
        source: z
          .enum(KNOWN_SOURCES)
          .optional()
          .describe("Restrict to one source."),
        sinceISO: z
          .string()
          .optional()
          .describe(
            "Only items with posted_at > this ISO timestamp. Typical use: " +
              "now - 24h for a daily digest.",
          ),
        untilISO: z
          .string()
          .optional()
          .describe("Only items with posted_at <= this ISO timestamp."),
        asOfISO: z
          .string()
          .optional()
          .describe(
            "Eval/judge point-in-time replay: only items already in the " +
              "store at this ISO instant (fetched_at <= asOfISO). Reconstructs " +
              "what the store held at a past moment, excluding rows fetched " +
              "later (poller lag). Normal runs omit this.",
          ),
        channel: z
          .string()
          .optional()
          .describe(
            "For source='channel' only: restrict to one Telegram channel " +
              "by chat_username or chat_id.",
          ),
        limit: z
          .number()
          .int()
          .min(1)
          .max(2000)
          .optional()
          .describe("Max rows. Default 500."),
        chunks: z.number().int().min(2).max(8).optional().describe(CHUNKS_DESCRIPTION),
      },
    },
    async ({ source, sinceISO, untilISO, asOfISO, channel, limit, chunks }) => {
      const items = await news.list({ source, sinceISO, untilISO, asOfISO, channel, limit });
      const serialized = items.map(serializeItem);
      if (chunks !== undefined) {
        return jsonResult({ count: items.length, chunks: splitChunks(serialized, chunks) });
      }
      return jsonResult({
        count: items.length,
        items: serialized,
      });
    },
  );

  server.registerTool(
    "fetch_article",
    {
      title: "Fetch and store an arbitrary article URL",
      description:
        "Download a web article (Mozilla Readability) and save it to the " +
        "news store so it becomes searchable. Manual override — the HN " +
        "and Habr pollers already cover their feeds. Use this for ad-hoc " +
        "URLs the user shares. Returns clean plaintext (title + body). " +
        "If the URL is already cached, returns the cached row without " +
        "re-fetching.",
      inputSchema: {
        url: z.string().url().describe("Article URL to fetch and extract."),
      },
    },
    async ({ url }) => {
      const cached = await findCachedArticle(news, url);
      if (cached && cached.body.trim().length > 0) {
        return jsonResult({
          url,
          title: cached.title ?? "",
          text: cached.body,
          source: cached.source,
          publishedAt: cached.postedAt?.toISOString() ?? undefined,
          sizeChars: cached.body.length,
          cached: true,
        });
      }

      const article = await fetchArticle(url);
      const source = sourceForUrl(url);
      const item: NewsItem = {
        source,
        externalId: url,
        title: article.title || null,
        url,
        body: article.text,
        metadata: { author: article.author, site: article.site },
        postedAt: article.publishedAt,
      };
      try {
        await news.upsert(item);
      } catch (err) {
        console.error(
          "[fetch_article] save step failed:",
          err instanceof Error ? err.message : err,
        );
      }
      return jsonResult({
        ...article,
        publishedAt: article.publishedAt?.toISOString() ?? undefined,
        source,
        sizeChars: article.text.length,
        cached: false,
      });
    },
  );
}


function serializeItem(i: NewsItem): Record<string, unknown> {
  return {
    source: i.source,
    externalId: i.externalId,
    title: i.title,
    url: i.url,
    body: i.body,
    metadata: i.metadata,
    postedAt: i.postedAt?.toISOString() ?? null,
  };
}

async function findCachedArticle(
  news: NewsRepository,
  url: string,
): Promise<NewsItem | null> {
  for (const source of ["hackernews", "habr", "external"]) {
    const cached = await news.findByExternalId(source, url);
    if (cached) return cached;
  }
  return null;
}

function sourceForUrl(url: string): string {
  try {
    const host = new URL(url).hostname;
    if (host.endsWith("habr.com")) return "habr";
    if (host.endsWith("ycombinator.com")) return "hackernews";
  } catch {
    // fall through
  }
  return "external";
}
