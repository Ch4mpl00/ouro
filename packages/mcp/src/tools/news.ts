import { z } from "zod";
import type { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { fetchHeadlines, fetchArticle, ALL_SOURCES } from "../services/news";
import { jsonResult } from "../result";

export function registerNewsTools(server: McpServer): void {
  server.registerTool(
    "list_news_headlines",
    {
      title: "List tech-news headlines",
      description:
        "Fetch top headlines from tech-news sources (Hacker News, Habr) — " +
        "titles, URLs, and metadata only, no article bodies. Use this first " +
        "to scan what's out there, filter by user interests, then call " +
        "fetch_article(url) for the items worth reading. Cheap and fast.",
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
        "full text of items you've chosen to summarize. Returns title, text, " +
        "site, publishedAt, author, and the character count of the body.",
      inputSchema: {
        url: z.string().url().describe("Article URL to fetch and extract."),
      },
    },
    async ({ url }) => {
      const article = await fetchArticle(url);
      return jsonResult({
        ...article,
        sizeChars: article.text.length,
      });
    },
  );
}
