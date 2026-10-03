import { extractFromHtml } from "@extractus/article-extractor";
import type { GuardedFetch } from "../../fetch";

export interface ExtractedArticle {
  url: string;
  title: string;
  text: string;
  site?: string;
  // Parsed Date or null — @extractus sometimes returns unparseable
  // date strings; we drop those rather than poison the news_items row
  // (drizzle's timestamp serializer throws on Invalid Date).
  publishedAt: Date | null;
  author?: string;
}

const TIMEOUT_MS = 20_000;

// Returns a cleaned plaintext rendition of an article. The page is fetched
// through the SSRF guard (article URLs come from the model and from feeds),
// then Mozilla Readability via @extractus/article-extractor extracts it. We
// strip any residual HTML the extractor may leave behind so the LLM gets a
// compact text blob.
export async function fetchArticle(fetcher: GuardedFetch, url: string): Promise<ExtractedArticle> {
  const { url: finalUrl, response } = await fetcher(url, {
    signal: AbortSignal.timeout(TIMEOUT_MS),
    headers: { accept: "text/html,application/xhtml+xml" },
  });
  if (!response.ok) {
    await response.body?.cancel();
    throw new Error(`fetching ${url} failed: HTTP ${response.status}`);
  }
  const article = await extractFromHtml(await response.text(), finalUrl);
  if (!article) throw new Error(`No article extracted from ${url}`);
  return {
    url,
    title: article.title ?? "",
    text: stripHtml(article.content ?? ""),
    site: article.source ?? undefined,
    publishedAt: parseDate(article.published),
    author: article.author ?? undefined,
  };
}

function parseDate(input: string | null | undefined): Date | null {
  if (!input) return null;
  const d = new Date(input);
  return Number.isNaN(d.getTime()) ? null : d;
}

// Two retries on top of the initial attempt (3 total). Returns null on
// terminal failure so the caller can drop the item.
export async function fetchArticleWithRetry(
  fetcher: GuardedFetch,
  url: string,
  attempts = 3,
): Promise<ExtractedArticle | null> {
  let lastErr: unknown;
  for (let i = 0; i < attempts; i++) {
    try {
      return await fetchArticle(fetcher, url);
    } catch (err) {
      lastErr = err;
      if (i < attempts - 1) {
        await new Promise((r) => setTimeout(r, 500 * (i + 1)));
      }
    }
  }
  console.warn(
    `[news-article] failed to fetch ${url} after ${attempts} attempts:`,
    lastErr instanceof Error ? lastErr.message : lastErr,
  );
  return null;
}

function stripHtml(html: string): string {
  return html
    .replace(/<script[\s\S]*?<\/script>/gi, " ")
    .replace(/<style[\s\S]*?<\/style>/gi, " ")
    .replace(/<[^>]+>/g, " ")
    .replace(/&nbsp;/g, " ")
    .replace(/&amp;/g, "&")
    .replace(/&lt;/g, "<")
    .replace(/&gt;/g, ">")
    .replace(/&quot;/g, '"')
    .replace(/&#39;/g, "'")
    .replace(/\s+/g, " ")
    .trim();
}
