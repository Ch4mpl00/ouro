import { and, asc, eq, gt, max, or, sql } from "drizzle-orm";
import type { Database } from "../../../db/pg/client";
import { newsItems } from "../../../db/pg/schema";

// Channel posts share the news_items table with HN/Habr articles.
// source='channel', external_id="<chat_id>:<tg_message_id>", and the
// channel-specific fields (chat_title, chat_username, views, forwards)
// live in metadata (jsonb).

export const CHANNEL_SOURCE = "channel" as const;

export interface ChannelPostRow {
  id: number;
  chat_id: string;
  chat_title: string | null;
  chat_username: string | null;
  tg_message_id: number;
  posted_at: string;
  text: string;
  views: number | null;
  forwards: number | null;
  fetched_at: string;
}

export interface ChannelPostInsert {
  chat_id: string;
  chat_title: string | null;
  chat_username: string | null;
  tg_message_id: number;
  posted_at: string;
  text: string;
  views: number | null;
  forwards: number | null;
}

interface ChannelMetadata {
  chat_id: string;
  chat_title: string | null;
  chat_username: string | null;
  tg_message_id: number;
  views: number | null;
  forwards: number | null;
}

export interface ListChannelPostsOpts {
  since: string; // ISO timestamp; posts with posted_at > since
  channel?: string; // matches metadata.chat_username or metadata.chat_id
  limit?: number;
}

export function channelExternalId(chatId: string, tgMessageId: number): string {
  return `${chatId}:${tgMessageId}`;
}

export interface ChannelStorage {
  // Returns the external_ids that were actually inserted, so the caller
  // can embed only the new rows.
  insertChannelPosts(rows: ChannelPostInsert[]): Promise<string[]>;
  // Returns null when we've never seen this channel.
  getChannelWatermark(chatId: string): Promise<number | null>;
  listChannelPosts(opts: ListChannelPostsOpts): Promise<ChannelPostRow[]>;
}

export function createChannelStorage(db: Database): ChannelStorage {
  const insertChannelPosts = async (rows: ChannelPostInsert[]): Promise<string[]> => {
    if (rows.length === 0) return [];
    const values = rows.map((r) => {
      const meta: ChannelMetadata = {
        chat_id: r.chat_id,
        chat_title: r.chat_title,
        chat_username: r.chat_username,
        tg_message_id: r.tg_message_id,
        views: r.views,
        forwards: r.forwards,
      };
      return {
        source: CHANNEL_SOURCE,
        externalId: channelExternalId(r.chat_id, r.tg_message_id),
        title: r.chat_title,
        url: r.chat_username
          ? `https://t.me/${r.chat_username}/${r.tg_message_id}`
          : null,
        body: r.text,
        metadata: meta,
        postedAt: new Date(r.posted_at),
      };
    });
    const inserted = await db
      .insert(newsItems)
      .values(values)
      .onConflictDoNothing({ target: [newsItems.source, newsItems.externalId] })
      .returning({ externalId: newsItems.externalId });
    return inserted.map((r) => r.externalId);
  };

  const getChannelWatermark = async (chatId: string): Promise<number | null> => {
    const rows = await db
      .select({
        maxId: max(sql<number>`(${newsItems.metadata} ->> 'tg_message_id')::int`),
      })
      .from(newsItems)
      .where(
        and(
          eq(newsItems.source, CHANNEL_SOURCE),
          sql`${newsItems.metadata} ->> 'chat_id' = ${chatId}`,
        ),
      );
    const value = rows[0]?.maxId;
    return value === undefined || value === null ? null : Number(value);
  };

  const listChannelPosts = async (
    opts: ListChannelPostsOpts,
  ): Promise<ChannelPostRow[]> => {
    const limit = opts.limit ?? 500;
    const filters = [
      eq(newsItems.source, CHANNEL_SOURCE),
      gt(newsItems.postedAt, new Date(opts.since)),
    ];
    if (opts.channel) {
      const channelFilter = or(
        sql`${newsItems.metadata} ->> 'chat_username' = ${opts.channel}`,
        sql`${newsItems.metadata} ->> 'chat_id' = ${opts.channel}`,
      );
      if (channelFilter) filters.push(channelFilter);
    }
    const rows = await db
      .select({
        id: newsItems.id,
        metadata: newsItems.metadata,
        body: newsItems.body,
        postedAt: newsItems.postedAt,
        fetchedAt: newsItems.fetchedAt,
      })
      .from(newsItems)
      .where(and(...filters))
      .orderBy(asc(newsItems.postedAt))
      .limit(limit);

    return rows.map((r) => {
      const meta = r.metadata as ChannelMetadata;
      return {
        id: Number(r.id),
        chat_id: meta.chat_id,
        chat_title: meta.chat_title,
        chat_username: meta.chat_username,
        tg_message_id: meta.tg_message_id,
        posted_at: r.postedAt!.toISOString(),
        text: r.body,
        views: meta.views,
        forwards: meta.forwards,
        fetched_at: r.fetchedAt.toISOString(),
      };
    });
  };

  return { insertChannelPosts, getChannelWatermark, listChannelPosts };
}
