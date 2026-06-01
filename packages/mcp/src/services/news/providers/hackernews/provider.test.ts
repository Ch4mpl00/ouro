import { describe, expect, it } from "vitest";
import { createHackerNewsProvider } from "./provider";
import type { HnHeadline } from "./api";

const HEADLINE: HnHeadline = {
  id: 1,
  title: "Headline title",
  url: "https://example.com/a",
  score: 100,
  comments: 25,
  author: "alice",
  postedAt: new Date("2026-01-01T00:00:00Z"),
};

describe("hackernews provider", () => {
  it("maps a happy-path article into a NewsItem", async () => {
    const provider = createHackerNewsProvider({
      fetchHeadlines: async () => [HEADLINE],
      fetchArticle: async () => ({
        url: HEADLINE.url,
        title: "Article title",
        text: "body content",
        site: "example.com",
        author: "bob",
        publishedAt: new Date("2026-01-02T00:00:00Z"),
      }),
    });
    const items = await provider.fetch();
    expect(items).toEqual([
      {
        source: "hackernews",
        externalId: HEADLINE.url,
        title: "Article title",
        url: HEADLINE.url,
        body: "body content",
        metadata: {
          hn_id: 1,
          score: 100,
          comments: 25,
          // Headline author wins when present.
          author: "alice",
          site: "example.com",
        },
        postedAt: new Date("2026-01-02T00:00:00Z"),
      },
    ]);
  });

  it("falls back to the headline title when extracted title is empty", async () => {
    const provider = createHackerNewsProvider({
      fetchHeadlines: async () => [HEADLINE],
      fetchArticle: async () => ({
        url: HEADLINE.url,
        title: "",
        text: "body",
        publishedAt: null,
      }),
    });
    const [item] = await provider.fetch();
    expect(item?.title).toBe("Headline title");
  });

  it("drops items whose extraction returned null", async () => {
    const provider = createHackerNewsProvider({
      fetchHeadlines: async () => [HEADLINE, { ...HEADLINE, id: 2, url: "https://b" }],
      fetchArticle: async (url) =>
        url === HEADLINE.url
          ? null
          : { url, title: "t", text: "ok body", publishedAt: null },
    });
    const items = await provider.fetch();
    expect(items).toHaveLength(1);
    expect(items[0]?.externalId).toBe("https://b");
  });

  it("drops items whose body is whitespace-only", async () => {
    const provider = createHackerNewsProvider({
      fetchHeadlines: async () => [HEADLINE],
      fetchArticle: async () => ({ url: HEADLINE.url, title: "t", text: "   \n  ", publishedAt: null }),
    });
    expect(await provider.fetch()).toEqual([]);
  });

  it("respects the limit opt when fetching headlines", async () => {
    let observed = -1;
    const provider = createHackerNewsProvider(
      {
        fetchHeadlines: async (limit) => {
          observed = limit;
          return [];
        },
        fetchArticle: async () => null,
      },
      { limit: 5 },
    );
    await provider.fetch();
    expect(observed).toBe(5);
  });
});
