import { describe, expect, it } from "vitest";
import { createHabrProvider } from "./provider";
import type { HabrHeadline } from "./api";

const HEADLINE: HabrHeadline = {
  title: "Habr headline",
  url: "https://habr.com/x",
  author: "alice",
  postedAt: new Date("2026-01-01T00:00:00Z"),
};

describe("habr provider", () => {
  it("maps a happy-path article into a NewsItem", async () => {
    const provider = createHabrProvider({
      fetchHeadlines: async () => [HEADLINE],
      fetchArticle: async () => ({
        url: HEADLINE.url,
        title: "Article title",
        text: "body",
        site: "habr.com",
        publishedAt: null,
      }),
    });
    const items = await provider.fetch();
    expect(items[0]).toMatchObject({
      source: "habr",
      externalId: HEADLINE.url,
      title: "Article title",
      url: HEADLINE.url,
      body: "body",
      metadata: { author: "alice", site: "habr.com" },
    });
  });

  it("drops items with null extraction or empty body", async () => {
    const provider = createHabrProvider({
      fetchHeadlines: async () => [
        HEADLINE,
        { ...HEADLINE, url: "https://habr.com/y" },
      ],
      fetchArticle: async (url) =>
        url === HEADLINE.url
          ? null
          : { url, title: "t", text: "", publishedAt: null },
    });
    expect(await provider.fetch()).toEqual([]);
  });
});
