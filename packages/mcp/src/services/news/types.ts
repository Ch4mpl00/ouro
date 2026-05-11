export type NewsSource = "hackernews" | "habr";

export interface NewsItem {
  source: NewsSource;
  title: string;
  url: string;
  score?: number;
  comments?: number;
  author?: string;
  publishedAt?: string;
}
