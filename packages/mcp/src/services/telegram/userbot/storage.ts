import { getDb } from "../../../db/client";

// Persistence layer for the userbot channel poller. Each row is one
// individual post from a subscribed Telegram channel; UNIQUE(chat_id,
// tg_message_id) makes the upsert idempotent across re-polls.

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

// Returns the number of rows actually inserted (duplicates ignored).
export function insertChannelPosts(rows: ChannelPostInsert[]): number {
  if (rows.length === 0) return 0;
  const stmt = getDb().prepare(
    `INSERT OR IGNORE INTO channel_posts
       (chat_id, chat_title, chat_username, tg_message_id, posted_at, text, views, forwards)
     VALUES (?, ?, ?, ?, ?, ?, ?, ?)`,
  );
  const insertMany = getDb().transaction((items: ChannelPostInsert[]) => {
    let inserted = 0;
    for (const r of items) {
      const info = stmt.run(
        r.chat_id,
        r.chat_title,
        r.chat_username,
        r.tg_message_id,
        r.posted_at,
        r.text,
        r.views,
        r.forwards,
      );
      if (info.changes > 0) inserted++;
    }
    return inserted;
  });
  return insertMany(rows);
}

// Watermark for the poller: highest tg_message_id we've already stored for
// this channel. Next poll uses minId=watermark to fetch only new posts.
// Returns null if we've never seen this channel — caller bootstraps with a
// small backfill window in that case.
export function getChannelWatermark(chatId: string): number | null {
  const row = getDb()
    .prepare(`SELECT MAX(tg_message_id) AS max_id FROM channel_posts WHERE chat_id = ?`)
    .get(chatId) as { max_id: number | null } | undefined;
  return row?.max_id ?? null;
}

export interface ListChannelPostsOpts {
  since: string;          // ISO timestamp; posts with posted_at > since
  channel?: string;       // optional filter — matches chat_username or chat_id
  limit?: number;
}

export function listChannelPosts(opts: ListChannelPostsOpts): ChannelPostRow[] {
  const limit = opts.limit ?? 500;
  const clauses: string[] = ["posted_at > ?"];
  const params: unknown[] = [opts.since];
  if (opts.channel) {
    clauses.push("(chat_username = ? OR chat_id = ?)");
    params.push(opts.channel, opts.channel);
  }
  const sql = `SELECT id, chat_id, chat_title, chat_username, tg_message_id,
                      posted_at, text, views, forwards, fetched_at
                 FROM channel_posts
                WHERE ${clauses.join(" AND ")}
                ORDER BY posted_at ASC
                LIMIT ?`;
  params.push(limit);
  return getDb().prepare(sql).all(...params) as ChannelPostRow[];
}
