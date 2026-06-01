import { sql } from "drizzle-orm";
import {
  bigserial,
  index,
  jsonb,
  pgTable,
  text,
  timestamp,
  unique,
  vector,
} from "drizzle-orm/pg-core";

// One row per piece of content (HN/Habr article, Telegram channel post).
// external_id is the natural id in its source: URL for HN/Habr,
// "<chat_id>:<tg_message_id>" for channels. body defaults to '' so we
// can land a title-only record from list_news_headlines and backfill
// the body later from fetch_article.

export const newsItems = pgTable(
  "news_items",
  {
    id: bigserial("id", { mode: "number" }).primaryKey(),
    source: text("source").notNull(),
    externalId: text("external_id").notNull(),
    title: text("title"),
    url: text("url"),
    body: text("body").notNull().default(""),
    metadata: jsonb("metadata").notNull().default(sql`'{}'::jsonb`),
    postedAt: timestamp("posted_at", { withTimezone: true }),
    fetchedAt: timestamp("fetched_at", { withTimezone: true })
      .notNull()
      .defaultNow(),
    embeddedAt: timestamp("embedded_at", { withTimezone: true }),
    embedding: vector("embedding", { dimensions: 1536 }),
  },
  (t) => [
    unique("news_items_source_external_uniq").on(t.source, t.externalId),
    index("news_items_posted_at").on(t.postedAt.desc()),
    index("news_items_source_posted").on(t.source, t.postedAt.desc()),
    index("news_items_embedding_ivf")
      .using("ivfflat", t.embedding.op("vector_cosine_ops"))
      .with({ lists: 100 }),
  ],
);

export type NewsItemRow = typeof newsItems.$inferSelect;
export type NewsItemInsert = typeof newsItems.$inferInsert;
