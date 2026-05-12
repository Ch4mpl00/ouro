import { getUserbotClient } from "./client";
import { getSavedSession } from "./auth";
import {
  getChannelWatermark,
  insertChannelPosts,
  type ChannelPostInsert,
} from "./storage";

// Background poller. Every ~30min, walks every channel-type dialog the
// userbot is subscribed to, fetches new posts via gramjs, and persists
// them into channel_posts (UNIQUE(chat_id, tg_message_id) de-dupes).
//
// First time we see a channel we backfill BOOTSTRAP_LIMIT recent posts,
// then subsequent polls use minId=last_seen so each poll only sees the
// delta. ~200ms pause between channels to stay clear of FLOOD_WAIT.

const DEFAULT_INTERVAL_MS = 30 * 60_000;
const BOOTSTRAP_LIMIT = 50;
const DELTA_LIMIT = 200;
const INTER_CHANNEL_DELAY_MS = 200;

function intervalMs(): number {
  const raw = process.env.USERBOT_POLL_INTERVAL_MS;
  if (!raw) return DEFAULT_INTERVAL_MS;
  const n = Number(raw);
  return Number.isFinite(n) && n >= 60_000 ? n : DEFAULT_INTERVAL_MS;
}

function logPrefix(): string {
  return `[${new Date().toISOString()}] [userbot-poller]`;
}

function sleep(ms: number): Promise<void> {
  return new Promise((r) => setTimeout(r, ms));
}

interface EntityLike {
  id?: unknown;
  title?: string;
  username?: string;
  firstName?: string;
}

interface RawMessage {
  id: number;
  date: number;
  message?: string;
  views?: number;
  forwards?: number;
}

async function pollOnce(): Promise<void> {
  const client = await getUserbotClient();
  const dialogs = await client.getDialogs({ limit: 500 });

  let totalChannels = 0;
  let totalInserted = 0;

  for (const d of dialogs) {
    if (!d.isChannel) continue;
    totalChannels++;

    const entity = d.entity as EntityLike | undefined;
    if (!entity || entity.id === undefined) continue;
    const chatId = String(entity.id);
    const chatTitle = d.title ?? entity.title ?? null;
    const chatUsername = entity.username ?? null;

    const watermark = getChannelWatermark(chatId);
    const limit = watermark === null ? BOOTSTRAP_LIMIT : DELTA_LIMIT;

    let raw: RawMessage[];
    try {
      raw = (await client.getMessages(d.entity, {
        limit,
        minId: watermark ?? undefined,
      })) as unknown as RawMessage[];
    } catch (err) {
      console.warn(
        `${logPrefix()} channel ${chatTitle ?? chatId}: fetch failed: ${(err as Error).message}`,
      );
      await sleep(INTER_CHANNEL_DELAY_MS);
      continue;
    }

    const rows: ChannelPostInsert[] = [];
    for (const m of raw) {
      const text = (m.message ?? "").trim();
      if (!text) continue;
      rows.push({
        chat_id: chatId,
        chat_title: chatTitle,
        chat_username: chatUsername,
        tg_message_id: Number(m.id),
        posted_at: new Date(Number(m.date) * 1000).toISOString(),
        text,
        views: typeof m.views === "number" ? m.views : null,
        forwards: typeof m.forwards === "number" ? m.forwards : null,
      });
    }

    const inserted = insertChannelPosts(rows);
    totalInserted += inserted;
    if (inserted > 0) {
      console.log(
        `${logPrefix()} ${chatTitle ?? chatId}: +${inserted}/${rows.length} (watermark=${watermark ?? "bootstrap"})`,
      );
    }
    await sleep(INTER_CHANNEL_DELAY_MS);
  }

  console.log(
    `${logPrefix()} tick complete: ${totalChannels} channels scanned, ${totalInserted} new posts`,
  );
}

export function startUserbotPoller(): void {
  if (!getSavedSession()) {
    console.warn(
      `${logPrefix()} no saved userbot session — poller disabled. Run \`pnpm userbot:auth\` to enable.`,
    );
    return;
  }
  const ms = intervalMs();
  console.log(`${logPrefix()} starting (interval ${Math.round(ms / 60_000)}min)`);

  // Fire one tick on boot (after a short delay so MCP transport is up),
  // then run on the interval. Each tick is async-wrapped so a thrown
  // error doesn't kill the setInterval.
  const tick = (): void => {
    void pollOnce().catch((err) => {
      console.error(`${logPrefix()} pollOnce crashed:`, err);
    });
  };
  setTimeout(tick, 10_000);
  setInterval(tick, ms);
}
