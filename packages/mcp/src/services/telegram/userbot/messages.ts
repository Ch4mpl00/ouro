import { Api } from "telegram";
import { getUserbotClient } from "./client";
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
export async function fetchChannelMessages(opts: {
  channel: string;
  sinceMessageId?: number;
  limit?: number;
}): Promise<{ channel: UserbotChannelInfo; messages: UserbotMessage[] }> {
  const client = await getUserbotClient();
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

export async function listDialogs(limit = 100): Promise<UserbotDialog[]> {
  const client = await getUserbotClient();
  const dialogs = await client.getDialogs({ limit });
  const out: UserbotDialog[] = [];
  for (const d of dialogs) {
    const entity = d.entity as { username?: string } | undefined;
    out.push({
      id: String(d.id),
      type: d.isChannel ? "channel" : d.isGroup ? "group" : d.isUser ? "user" : "unknown",
      title: d.title ?? d.name ?? "(untitled)",
      username: entity?.username,
      unreadCount: d.unreadCount,
    });
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
