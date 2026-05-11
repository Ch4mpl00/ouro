import { extract } from "@extractus/article-extractor";

export interface ExtractedArticle {
  url: string;
  title: string;
  text: string;
  site?: string;
  publishedAt?: string;
  author?: string;
}

// Returns a cleaned plaintext rendition of an article. Uses Mozilla
// Readability via @extractus/article-extractor under the hood. We strip
// any residual HTML the extractor may leave behind so the LLM gets a
// compact text blob (the agent has 1M context, but article bodies still
// shouldn't carry markup noise).
export async function fetchArticle(url: string): Promise<ExtractedArticle> {
  const article = await extract(url);
  if (!article) throw new Error(`No article extracted from ${url}`);
  return {
    url,
    title: article.title ?? "",
    text: stripHtml(article.content ?? ""),
    site: article.source ?? undefined,
    publishedAt: article.published ?? undefined,
    author: article.author ?? undefined,
  };
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
