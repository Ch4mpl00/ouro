import "dotenv/config";
import path from "node:path";
import Database from "better-sqlite3";
import { createPgClient } from "../db/pg/client";
import { createNewsModule, type NewsItem } from "../services/news";

// One-shot migration of legacy sqlite `channel_posts` rows into the
// news store, with inline embedding. Idempotent. Reads the TS-era
// `tokens.db` (MCP_DB_PATH, or packages/mcp/data/tokens.db) read-only:
//
//   pnpm db:migrate:channel-posts

const LEGACY_DB = process.env.MCP_DB_PATH ?? path.resolve(import.meta.dirname, "../../data/tokens.db");

interface OldRow {
  chat_id: string;
  chat_title: string | null;
  chat_username: string | null;
  tg_message_id: number;
  posted_at: string;
  text: string;
  views: number | null;
  forwards: number | null;
}

const BATCH = 100;

function toNewsItem(r: OldRow): NewsItem {
  return {
    source: "channel",
    externalId: `${r.chat_id}:${r.tg_message_id}`,
    title: null,
    url: r.chat_username
      ? `https://t.me/${r.chat_username}/${r.tg_message_id}`
      : null,
    body: r.text,
    metadata: {
      chat_id: r.chat_id,
      chat_title: r.chat_title,
      chat_username: r.chat_username,
      tg_message_id: r.tg_message_id,
      views: r.views,
      forwards: r.forwards,
    },
    postedAt: new Date(r.posted_at),
  };
}

async function main(): Promise<void> {
  const pg = createPgClient();
  await pg.ensureReady();
  const { repository: news } = createNewsModule({ db: pg.db });

  const sqlite = new Database(LEGACY_DB, { readonly: true, fileMustExist: true });
  try {
    const exists = sqlite
      .prepare(`SELECT 1 FROM sqlite_master WHERE type='table' AND name='channel_posts'`)
      .get();
    if (!exists) {
      console.log("[migrate] sqlite channel_posts table not present, nothing to do");
      return;
    }
    const all = sqlite
      .prepare(
        `SELECT chat_id, chat_title, chat_username, tg_message_id, posted_at,
                text, views, forwards
           FROM channel_posts
          ORDER BY posted_at ASC`,
      )
      .all() as OldRow[];
    console.log(`[migrate] sqlite has ${all.length} rows to migrate`);

    let saved = 0;
    let embeddedTotal = 0;
    let failedTotal = 0;
    for (let i = 0; i < all.length; i += BATCH) {
      const slice = all.slice(i, i + BATCH);
      const items = slice.map(toNewsItem);
      const result = await news.save(items);
      saved += result.saved;
      embeddedTotal += result.embedded;
      failedTotal += result.failed;
      console.log(
        `[migrate] batch ${i + slice.length}/${all.length} (saved=${result.saved}, embedded=${embeddedTotal}, failed=${failedTotal})`,
      );
    }
    console.log(
      `[migrate] done: saved=${saved}, embedded=${embeddedTotal}, embed-failed=${failedTotal}`,
    );
  } finally {
    sqlite.close();
    await pg.close();
  }
}

main().catch((err) => {
  console.error("[migrate] crashed:", err);
  process.exitCode = 1;
});
