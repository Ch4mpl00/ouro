import { Api } from "telegram";
import type { Userbot } from "./client";
import { normalizeHandle } from "./channels";

export interface UserbotMessage {
  id: number;
  date: string;
  text: string;
  views?: number;
  forwards?: number;
}

export interface UserbotChannelInfo {
  id: string;
  title: string;
  username?: string;
}

export interface UserbotDialog {
  id: string;
  type: "user" | "group" | "channel" | "unknown";
  title: string;
  username?: string;
  unreadCount: number;
}

// Fetch the most recent N messages from a channel/group. If `sinceMessageId`
// is set, only messages with id > sinceMessageId are returned (gramjs uses
// `minId` semantics which exclude the boundary). Without it, returns the
// last `limit` messages.
export async function fetchChannelMessages(userbot: Userbot, opts: {
  channel: string;
  sinceMessageId?: number;
  limit?: number;
}): Promise<{ channel: UserbotChannelInfo; messages: UserbotMessage[] }> {
  const client = await userbot.client();
  const handle = normalizeHandle(opts.channel);
  const entity = await client.getEntity(handle);

  const raw = await client.getMessages(entity, {
    limit: opts.limit ?? 30,
    minId: opts.sinceMessageId,
  });

  const messages: UserbotMessage[] = [];
  for (const m of raw) {
    const text = (m.message ?? "").trim();
    if (!text) continue;
    messages.push({
      id: Number(m.id),
      date: new Date(Number(m.date) * 1000).toISOString(),
      text,
      views: typeof m.views === "number" ? m.views : undefined,
      forwards: typeof m.forwards === "number" ? m.forwards : undefined,
    });
  }
  // gramjs returns newest-first; reverse for chronological order.
  messages.reverse();

  return { channel: entityToChannelInfo(entity), messages };
}

// What listing dialogs needs from the client — the real TelegramClient
// satisfies it structurally, and a test can hand in a stand-in.
export interface DialogsClient {
  getDialogs(params: { limit?: number }): Promise<Iterable<DialogEntry>>;
}

interface DialogEntry {
  id?: unknown;
  // Telegram reports a supergroup as a channel too: `isChannel` for every
  // channel-type chat, `isGroup` for groups including supergroups.
  isChannel: boolean;
  isGroup: boolean;
  isUser: boolean;
  title?: string;
  name?: string;
  entity?: unknown;
  unreadCount: number;
}

// The account's dialogs in Telegram's order (most recent first). With a
// type, the first `limit` dialogs OF THAT TYPE — filtering after the limit
// would hand back fewer than exist.
export async function listDialogs(
  source: { client(): Promise<DialogsClient> },
  opts: { type?: UserbotDialog["type"]; limit?: number } = {},
): Promise<UserbotDialog[]> {
  const limit = opts.limit ?? 100;
  const client = await source.client();
  const dialogs = await client.getDialogs(opts.type ? {} : { limit });
  const out: UserbotDialog[] = [];
  for (const d of dialogs) {
    const type: UserbotDialog["type"] = d.isGroup ? "group" : d.isChannel ? "channel" : d.isUser ? "user" : "unknown";
    if (opts.type && type !== opts.type) continue;
    const username: unknown = typeof d.entity === "object" && d.entity !== null ? Reflect.get(d.entity, "username") : undefined;
    out.push({
      id: String(d.id),
      type,
      title: d.title ?? d.name ?? "(untitled)",
      username: typeof username === "string" ? username : undefined,
      unreadCount: d.unreadCount,
    });
    if (out.length === limit) break;
  }
  return out;
}

function entityToChannelInfo(entity: Api.TypeEntityLike | unknown): UserbotChannelInfo {
  const e = entity as { id?: unknown; title?: string; username?: string; firstName?: string };
  return {
    id: String(e.id ?? "?"),
    title: e.title ?? e.firstName ?? "(unknown)",
    username: e.username,
  };
}
