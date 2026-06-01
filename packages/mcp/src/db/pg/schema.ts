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
// "<chat_id>:<tg_message_id>" for channels.

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
  // No ANN index on `embedding`: ivfflat with low row counts (<10k)
  // is effectively a random filter — small clusters + probes=1 default
  // means most relevant rows live in clusters the query never visits.
  // Sequential scan + cosineDistance on 1536-dim vectors is sub-ms at
  // this scale. Add ivfflat (or hnsw) back when the corpus crosses
  // ~50–100k rows; tune `lists` ≈ sqrt(rows) and bump probes per query.
  (t) => [
    unique("news_items_source_external_uniq").on(t.source, t.externalId),
    index("news_items_posted_at").on(t.postedAt.desc()),
    index("news_items_source_posted").on(t.source, t.postedAt.desc()),
  ],
);

export type NewsItemRow = typeof newsItems.$inferSelect;
export type NewsItemInsert = typeof newsItems.$inferInsert;
