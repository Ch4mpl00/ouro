import type { Userbot } from "../../../telegram/userbot";
import type { ChannelHandle, ChannelMessage, UserbotChannelsAdapter } from "./adapter";

// The default adapter: the logged-in userbot's gramjs client.
//
// It needs two calls from the client, so it asks for just those — the real
// TelegramClient satisfies this structurally, and a test can hand in a
// stand-in that behaves like Telegram does.
export interface ChannelsClient {
  getDialogs(params: { limit: number }): Promise<Iterable<DialogLike>>;
  getMessages(entity: unknown, params: { limit: number; minId?: number; reverse?: boolean }): Promise<Iterable<MessageLike>>;
}

interface DialogLike {
  isChannel: boolean;
  title?: string;
  entity?: unknown;
}

interface MessageLike {
  id: number;
  date: number;
  message?: string;
  views?: number;
  forwards?: number;
}

export interface ChannelsAdapterDeps {
  hasSession(): Promise<boolean>;
  client(): Promise<ChannelsClient>;
}

export function createGramjsUserbotAdapter(userbot: Userbot): UserbotChannelsAdapter {
  return createChannelsAdapter({ hasSession: () => userbot.hasSession(), client: () => userbot.client() });
}

export function createChannelsAdapter(deps: ChannelsAdapterDeps): UserbotChannelsAdapter {
  return {
    hasSession: () => deps.hasSession(),

    listChannels: async (): Promise<ChannelHandle[]> => {
      const client = await deps.client();
      const out: ChannelHandle[] = [];
      for (const d of await client.getDialogs({ limit: 500 })) {
        if (!d.isChannel || typeof d.entity !== "object" || d.entity === null) continue;
        const id: unknown = Reflect.get(d.entity, "id");
        if (id === undefined || id === null) continue;
        const title: unknown = Reflect.get(d.entity, "title");
        const username: unknown = Reflect.get(d.entity, "username");
        out.push({
          chatId: String(id),
          title: d.title ?? (typeof title === "string" ? title : null),
          username: typeof username === "string" ? username : null,
          ref: d.entity,
        });
      }
      return out;
    },

    fetchMessages: async (channel, opts): Promise<ChannelMessage[]> => {
      const client = await deps.client();
      // After a watermark: the OLDEST `limit` posts past it (`reverse`), so a
      // channel that posted more than a tick's worth is caught up over the
      // next ticks. Telegram's default order is newest first — that stored
      // the newest `limit`, moved the watermark past them, and the older
      // ones in between were never fetched. Bootstrap: the newest `limit`.
      const raw = await client.getMessages(
        channel.ref,
        opts.sinceMessageId === null
          ? { limit: opts.limit }
          : { limit: opts.limit, minId: opts.sinceMessageId, reverse: true },
      );
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
