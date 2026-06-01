import { getSavedSession } from "../../../telegram/userbot";
import { getUserbotClient } from "../../../telegram/userbot";
import type {
  ChannelHandle,
  ChannelMessage,
  UserbotChannelsAdapter,
} from "./adapter";

// Default gramjs-backed implementation of UserbotChannelsAdapter.

interface EntityLike {
  id?: unknown;
  title?: string;
  username?: string;
}

interface RawMessage {
  id: number;
  date: number;
  message?: string;
  views?: number;
  forwards?: number;
}

export function createGramjsUserbotAdapter(): UserbotChannelsAdapter {
  return {
    hasSession: () => getSavedSession() !== null,

    listChannels: async (): Promise<ChannelHandle[]> => {
      const client = await getUserbotClient();
      const dialogs = await client.getDialogs({ limit: 500 });
      const out: ChannelHandle[] = [];
      for (const d of dialogs) {
        if (!d.isChannel) continue;
        const entity = d.entity as EntityLike | undefined;
        if (!entity || entity.id === undefined) continue;
        out.push({
          chatId: String(entity.id),
          title: d.title ?? entity.title ?? null,
          username: entity.username ?? null,
          ref: d.entity,
        });
      }
      return out;
    },

    fetchMessages: async (channel, opts): Promise<ChannelMessage[]> => {
      const client = await getUserbotClient();
      const raw = (await client.getMessages(channel.ref as Parameters<typeof client.getMessages>[0], {
        limit: opts.limit,
        minId: opts.sinceMessageId ?? undefined,
      })) as unknown as RawMessage[];
      const out: ChannelMessage[] = [];
      for (const m of raw) {
        const text = (m.message ?? "").trim();
        if (!text) continue;
        out.push({
          id: Number(m.id),
          date: new Date(Number(m.date) * 1000),
          text,
          views: typeof m.views === "number" ? m.views : null,
          forwards: typeof m.forwards === "number" ? m.forwards : null,
        });
      }
      return out;
    },
  };
}
