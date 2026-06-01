import "dotenv/config";
import { getDb, closeDb } from "../db/client";
import { createPgClient } from "../db/pg/client";
import {
  CHANNEL_SOURCE,
  createChannelStorage,
  type ChannelPostInsert,
} from "../services/telegram/userbot/storage";
import { createNewsModule } from "../services/news/module";

// One-shot migration of legacy sqlite `channel_posts` rows into PG
// `news_items`, with inline embedding. Idempotent via UNIQUE(source,
// external_id). Run inside the mcp container:
//
//   docker compose exec -w /app mcp pnpm db:migrate:channel-posts

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

async function main(): Promise<void> {
  const pg = createPgClient();
  await pg.ensureReady();
  const news = createNewsModule({ db: pg.db });
  const channelStorage = createChannelStorage(pg.db);

  const sqlite = getDb();
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

    let inserted = 0;
    let embeddedTotal = 0;
    let failedTotal = 0;
    for (let i = 0; i < all.length; i += BATCH) {
      const slice = all.slice(i, i + BATCH);
      const batch: ChannelPostInsert[] = slice.map((r) => ({
        chat_id: r.chat_id,
        chat_title: r.chat_title,
        chat_username: r.chat_username,
        tg_message_id: r.tg_message_id,
        posted_at: r.posted_at,
        text: r.text,
        views: r.views,
        forwards: r.forwards,
      }));
      const newExternalIds = await channelStorage.insertChannelPosts(batch);
      inserted += newExternalIds.length;
      if (newExternalIds.length > 0) {
        const result = await news.embeddings.embedByTargets(
          newExternalIds.map((externalId) => ({ source: CHANNEL_SOURCE, externalId })),
        );
        embeddedTotal += result.embedded;
        failedTotal += result.failed;
      }
      console.log(
        `[migrate] batch ${i + slice.length}/${all.length} (new=${newExternalIds.length}, embedded=${embeddedTotal}, failed=${failedTotal})`,
      );
    }
    console.log(
      `[migrate] done: inserted=${inserted}, embedded=${embeddedTotal}, embed-failed=${failedTotal}`,
    );
  } finally {
    closeDb();
    await pg.close();
  }
}

main().catch((err) => {
  console.error("[migrate] crashed:", err);
  process.exitCode = 1;
});
