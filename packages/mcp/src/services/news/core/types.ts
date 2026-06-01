// Unified news item — what providers return and what storage stores.
// `source` is provider-defined (hackernews / habr / channel / future).
// `externalId` is the natural key in that source's namespace: URL for
// HN/Habr articles, "<chat_id>:<tg_message_id>" for channel posts.

export interface NewsItem {
  source: string;
  externalId: string;
  title: string | null;
  url: string | null;
  body: string;
  metadata: Record<string, unknown>;
  postedAt: Date | null;
}

export interface SaveResult {
  saved: number;
  embedded: number;
  failed: number;
}

export interface ListOpts {
  source?: string;
  sinceISO?: string;
  untilISO?: string;
  limit?: number;
  // Channel-only convenience: matches metadata.chat_username OR chat_id.
  channel?: string;
}
