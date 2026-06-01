import { z } from "zod";
import type { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { fetchHeadlines, fetchArticle, ALL_SOURCES } from "../services/news";
import type { NewsItemMetadata } from "../services/news/storage";
import type { NewsModule } from "../services/news/module";
import { jsonResult } from "../result";
import type { NewsSource } from "../services/news";

export function registerNewsTools(server: McpServer, news: NewsModule): void {
  server.registerTool(
    "list_news_headlines",
    {
      title: "List tech-news headlines",
      description:
        "Fetch top headlines from tech-news sources (Hacker News, Habr) — " +
        "titles, URLs, and metadata only, no article bodies. Use this first " +
        "to scan what's out there, filter by user interests, then call " +
        "fetch_article(url) for the items worth reading. Headlines land in " +
        "the news_items store (Postgres + pgvector) under the hood and are " +
        "embedded inline, so they're discoverable via search_news.",
      inputSchema: {
        source: z
          .enum(["hackernews", "habr"])
          .optional()
          .describe(
            `Restrict to a single source. Omit to fetch from all (${ALL_SOURCES.join(", ")}).`,
          ),
        limit: z
          .number()
          .int()
          .min(1)
          .max(100)
          .optional()
          .describe("Headlines per source. Default 30."),
      },
    },
    async ({ source, limit }) => {
      const items = await fetchHeadlines({ source, limit });
      // Cache + embed are best-effort: the tool still returns the live
      // API response if either step fails.
      try {
        const newExternalIds = await news.storage.upsertHeadlines(items);
        if (newExternalIds.length > 0) {
          const novel = new Set(newExternalIds);
          const targets = items
            .filter((i) => novel.has(i.url))
            .map((i) => ({ source: i.source, externalId: i.url }));
          await news.embeddings.embedByTargets(targets);
        }
      } catch (err) {
        console.error(
          "[list_news_headlines] persistence/embed step failed:",
          err instanceof Error ? err.message : err,
        );
      }
      return jsonResult({ count: items.length, items });
    },
  );

  server.registerTool(
    "fetch_article",
    {
      title: "Fetch and extract an article",
      description:
        "Download a web article and return clean plaintext (title + body) " +
        "via Mozilla Readability. Use after list_news_headlines to read the " +
        "full text of items you've chosen to summarize. The body is cached " +
        "in news_items (Postgres) so re-fetching the same URL is free and " +
        "the article becomes searchable via search_news.",
      inputSchema: {
        url: z.string().url().describe("Article URL to fetch and extract."),
        source: z
          .enum(["hackernews", "habr"])
          .optional()
          .describe(
            "News source the URL came from. Used as the (source, url) " +
              "cache namespace. Defaults to whichever source has a cached " +
              "row for this URL, or 'hackernews' otherwise.",
          ),
      },
    },
    async ({ url, source }) => {
      const resolvedSource: NewsSource = source ?? (await guessSource(url, news));
      const cached = await news.storage.getCachedArticle(resolvedSource, url);
      if (cached && cached.body.trim().length > 0) {
        return jsonResult({
          url,
          title: cached.title ?? "",
          text: cached.body,
          site: cached.metadata.site,
          publishedAt: cached.postedAt ?? undefined,
          author: cached.metadata.author,
          sizeChars: cached.body.length,
          cached: true,
        });
      }

      const article = await fetchArticle(url);

      // drizzle's set overwrites the whole jsonb column, so merge here
      // to keep headline-time fields (score/comments) alongside the
      // article-time fields (site).
      const mergedMetadata: NewsItemMetadata = {
        ...(cached?.metadata ?? {}),
        author: article.author ?? cached?.metadata.author,
        site: article.site ?? cached?.metadata.site,
      };

      try {
        await news.storage.upsertArticleBody({
          source: resolvedSource,
          url,
          article,
          mergedMetadata,
        });
        await news.embeddings.embedByTargets(
          [{ source: resolvedSource, externalId: url }],
          { force: true }, // body just changed — replace stale title-only vector
        );
      } catch (err) {
        console.error(
          "[fetch_article] persistence/embed step failed:",
          err instanceof Error ? err.message : err,
        );
      }

      return jsonResult({
        ...article,
        sizeChars: article.text.length,
        cached: false,
      });
    },
  );

  server.registerTool(
    "search_news",
    {
      title: "Semantic search over the news store",
      description:
        "Vector search across every news item we've ingested (Hacker News, " +
        "Habr, harvested Telegram channels). Embeds the query with " +
        "text-embedding-3-small and returns the closest matches by cosine " +
        "distance. Use this when the user asks about a topic that may have " +
        "been mentioned earlier than the most recent digest, or to pull " +
        "cross-source context. Returns id, source, title, url, snippet " +
        "(first ~400 chars of the body), posted_at, distance (lower = " +
        "closer), and source-specific metadata.",
      inputSchema: {
        query: z.string().min(1).describe("Natural-language search query."),
        k: z
          .number()
          .int()
          .min(1)
          .max(50)
          .optional()
          .describe("Number of results to return. Default 10."),
        source: z
          .enum(["hackernews", "habr", "channel"])
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
        chatId: z
          .string()
          .optional()
          .describe(
            "For source='channel' only: restrict to one Telegram channel " +
              "by chat_id (or chat_username; match runs against both).",
          ),
      },
    },
    async ({ query, k, source, sinceISO, untilISO, chatId }) => {
      const results = await news.search({
        query,
        k,
        filter: { source, sinceISO, untilISO, chatId },
      });
      return jsonResult({ count: results.length, results });
    },
  );
}

// Only affects the (source, url) cache key, not the extraction itself.
async function guessSource(url: string, news: NewsModule): Promise<NewsSource> {
  for (const source of ALL_SOURCES) {
    const cached = await news.storage.getCachedArticle(source, url);
    if (cached) return source;
  }
  return "hackernews";
}
