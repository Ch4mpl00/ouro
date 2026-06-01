import { and, eq, max, sql } from "drizzle-orm";
import type { Database } from "../../../../db/pg/client";
import { newsItems } from "../../../../db/pg/schema";
import type { NewsProvider } from "../../core/provider";
import type { NewsItem } from "../../core/types";
import type { UserbotChannelsAdapter } from "./adapter";
import { createGramjsUserbotAdapter } from "./userbot-adapter";

// Telegram channel provider: walks every channel dialog the userbot is
// subscribed to and emits new posts as NewsItems with source='channel'.
//
// Per-channel watermark = max tg_message_id stored for that chat_id.
// First time we see a channel → BOOTSTRAP_LIMIT recent posts; thereafter
// DELTA_LIMIT with minId. ~200ms pause between channels to stay clear
// of FLOOD_WAIT. Returns [] (and logs a warning) if the userbot is
// not authorized — provider still ticks but produces nothing.

const SOURCE = "channel";
const DEFAULT_CADENCE_MS = 30 * 60_000;
const BOOTSTRAP_LIMIT = 50;
const DELTA_LIMIT = 200;
const INTER_CHANNEL_DELAY_MS = 200;

export type WatermarkReader = (chatId: string) => Promise<number | null>;

export interface TelegramChannelsProviderDeps {
  userbot: UserbotChannelsAdapter;
  getWatermark: WatermarkReader;
}

export interface TelegramChannelsProviderOpts {
  cadenceMs?: number;
  interChannelDelayMs?: number;
}

export function createPgWatermarkReader(db: Database): WatermarkReader {
  return async (chatId) => {
    const rows = await db
      .select({
        maxId: max(sql<number>`(${newsItems.metadata} ->> 'tg_message_id')::int`),
      })
      .from(newsItems)
      .where(
        and(
          eq(newsItems.source, SOURCE),
          sql`${newsItems.metadata} ->> 'chat_id' = ${chatId}`,
        ),
      );
    const value = rows[0]?.maxId;
    return value === undefined || value === null ? null : Number(value);
  };
}

export function defaultTelegramChannelsDeps(
  db: Database,
): TelegramChannelsProviderDeps {
  return {
    userbot: createGramjsUserbotAdapter(),
    getWatermark: createPgWatermarkReader(db),
  };
}

function externalId(chatId: string, tgMessageId: number): string {
  return `${chatId}:${tgMessageId}`;
}

const sleep = (ms: number): Promise<void> => new Promise((r) => setTimeout(r, ms));

export function createTelegramChannelsProvider(
  deps: TelegramChannelsProviderDeps,
  opts: TelegramChannelsProviderOpts = {},
): NewsProvider {
  const cadenceMs = opts.cadenceMs ?? DEFAULT_CADENCE_MS;
  const interChannelDelayMs = opts.interChannelDelayMs ?? INTER_CHANNEL_DELAY_MS;
  const { userbot, getWatermark } = deps;

  return {
    source: SOURCE,
    cadenceMs,
    fetch: async (): Promise<NewsItem[]> => {
      if (!userbot.hasSession()) {
        console.warn(
          "[news/telegram-channels] no saved userbot session — run `pnpm userbot:auth`. Skipping tick.",
        );
        return [];
      }

      const channels = await userbot.listChannels();
      const collected: NewsItem[] = [];

      for (const ch of channels) {
        const watermark = await getWatermark(ch.chatId);
        const limit = watermark === null ? BOOTSTRAP_LIMIT : DELTA_LIMIT;

        let messages;
        try {
          messages = await userbot.fetchMessages(ch, {
            sinceMessageId: watermark,
            limit,
          });
        } catch (err) {
          console.warn(
            `[news/telegram-channels] ${ch.title ?? ch.chatId}: fetch failed: ${(err as Error).message}`,
          );
          if (interChannelDelayMs > 0) await sleep(interChannelDelayMs);
          continue;
        }

        for (const m of messages) {
          collected.push({
            source: SOURCE,
            externalId: externalId(ch.chatId, m.id),
            title: ch.title,
            url: ch.username ? `https://t.me/${ch.username}/${m.id}` : null,
            body: m.text,
            metadata: {
              chat_id: ch.chatId,
              chat_title: ch.title,
              chat_username: ch.username,
              tg_message_id: m.id,
              views: m.views,
              forwards: m.forwards,
            },
            postedAt: m.date,
          });
        }
        if (interChannelDelayMs > 0) await sleep(interChannelDelayMs);
      }
      return collected;
    },
  };
}

export type { UserbotChannelsAdapter, ChannelHandle, ChannelMessage } from "./adapter";
