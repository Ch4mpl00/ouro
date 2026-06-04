export { createNewsModule, startNewsModule, type NewsModule } from "./module";
export type { NewsRepository } from "./core/repository";
export type { NewsProvider } from "./core/provider";
export type { NewsItem, ListOpts, SaveResult } from "./core/types";
export type {
  SearchOpts,
  SearchManyOpts,
  SearchResult,
  SearchFilter,
} from "./core/repository";
export type { EmbedResult } from "./core/embedder";
export { fetchArticle, type ExtractedArticle } from "./core/article";
